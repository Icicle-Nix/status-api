# Runs the hardened unit for real: the sandbox must not break systemctl
# (sshd "active") or journalctl (a provoked "Invalid user" shows up in
# ssh24h.reject). RAPL/GPU don't exist in the VM, so those are null.
{ self }:
{ pkgs, ... }:
let
  common = {
    imports = [ self.nixosModules.default ];
    services.openssh.enable = true;
    environment.systemPackages = [
      pkgs.curl
      pkgs.jq
    ];
  };
in
{
  name = "status-api";

  nodes.unix = {
    imports = [ common ];
    services.status-api = {
      enable = true;
      socketGroup = "web";
      disks = [
        "/"
        "/nix/store"
      ];
    };
    users.groups.web = { };
    users.users.member = {
      isNormalUser = true;
      extraGroups = [ "web" ];
    };
    users.users.outsider.isNormalUser = true;
  };

  nodes.tcp = {
    imports = [ common ];
    services.status-api = {
      enable = true;
      socket = "127.0.0.1:8435";
    };
  };

  testScript = ''
    import json

    start_all()

    def check_shape(s, disks):
        assert isinstance(s["host"], str) and s["host"], s
        assert isinstance(s["time"], int), s
        assert s["sshd"] == "active", s["sshd"]
        assert isinstance(s["ssh24h"]["ok"], int), s
        assert isinstance(s["ssh24h"]["reject"], int), s
        assert isinstance(s["uptime"], float) and s["uptime"] > 0, s
        assert len(s["load"]) == 3, s
        assert set(s["power"]) == {"cpu", "gpu", "gpuAsleep"}, s
        assert isinstance(s["power"]["gpuAsleep"], bool), s
        m = s["mem"]
        assert m["total"] > 0 and m["used"] == m["total"] - m["available"], m
        assert [d["mount"] for d in s["disks"]] == disks, s["disks"]
        for d in s["disks"]:
            assert 0 <= d["usedPct"] <= 100 and d["avail"] > 0, d
        ifaces = [n["iface"] for n in s["net"]]
        assert "lo" not in ifaces and ifaces == sorted(ifaces), ifaces
        assert "eth1" in ifaces, ifaces
        for n in s["net"]:
            assert (n["rxRate"] is None) == (n["txRate"] is None), n

    with subtest("unix socket: shape, sshd, journal"):
        unix.wait_for_unit("sshd.service")
        unix.wait_for_unit("status-api.socket")
        # Provoke a rejected login so the journal count is provably read.
        unix.succeed(
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=no nosuchuser@localhost true 2>&1 || true"
        )
        unix.wait_until_succeeds("journalctl -u sshd -o cat | grep -q 'Invalid user'")
        out = unix.succeed(
            "su member -c 'curl -sSf --unix-socket /run/status-api.sock http://localhost/status.json'"
        )
        s = json.loads(out)
        print(json.dumps(s, indent=2))
        check_shape(s, ["/", "/nix/store"])
        assert s["ssh24h"]["reject"] >= 1, s["ssh24h"]

    with subtest("unix socket: permissions and hardening"):
        unix.succeed('test "$(stat -c %G:%a /run/status-api.sock)" = web:660')
        unix.fail(
            "su outsider -c 'curl -sSf --unix-socket /run/status-api.sock http://localhost/'"
        )
        unix.succeed("systemctl show -p IPAddressDeny status-api | grep -q 'IPAddressDeny=.\\+'")
        print(unix.succeed("systemd-analyze security status-api | tail -1"))

    with subtest("unix socket: HTTP edge cases"):
        c = "curl -s --unix-socket /run/status-api.sock -o /dev/null -w '%{http_code}' "
        assert unix.succeed(c + "http://localhost/") == "200"
        assert unix.succeed(c + "http://localhost/nope") == "404"
        assert unix.succeed(c + "-X POST http://localhost/status.json") == "405"
        head = unix.succeed("curl -sSI --unix-socket /run/status-api.sock http://localhost/")
        for h in ["content-type: application/json", "cache-control: no-store", "connection: close"]:
            assert h in head.lower(), head

    with subtest("tcp socket"):
        tcp.wait_for_unit("sshd.service")
        tcp.wait_for_unit("status-api.socket")
        s = json.loads(tcp.succeed("curl -sSf http://127.0.0.1:8435/status.json"))
        check_shape(s, ["/"])
  '';
}
