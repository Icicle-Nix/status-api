# status-api

머신의 현재 상태(전력 · 부하 · 메모리 · 디스크 · 네트워크 · sshd)를 JSON으로
돌려주는 작은 HTTP 서버. 요청마다 그 자리에서 측정한다 — 타이머도, 상태
파일도 없다. 프론트엔드는 [Icicle-Nix/status-web](https://github.com/Icicle-Nix/status-web).

- Rust, 의존성은 `serde_json`과 `libc`뿐. HTTP 프레임워크 없이 std 위에
  최소한의 HTTP/1.1 응답기를 직접 구현했다 (요청 헤드 8 KiB 상한, 읽기 타임아웃
  5초, 연결당 스레드, 동시 처리 8개 초과 시 503, 항상 `Connection: close`).
- `GET`/`HEAD` `/` 또는 `/status.json` → 200. 그 밖의 경로는 404, 그 밖의
  메서드는 405 (작은 JSON 에러 본문).
- 한 요청은 약 1초 걸린다: RAPL 에너지와 네트워크 카운터를 샘플링하고 1초 잔
  뒤 다시 샘플링해서, CPU 전력과 네트워크 속도를 같은 구간에서 계산한다.
  sshd 질의(`systemctl`, `journalctl`)는 그 1초 동안 별도 스레드에서 돈다.

```
nix build                                   # → result/bin/status-api
nix flake check                             # 패키지 + NixOS VM 테스트
status-api --listen 127.0.0.1:8080 --disk / --disk /home   # 개발용 TCP
status-api --listen /tmp/status.sock                       # 개발용 Unix 소켓
```

systemd 소켓 활성화(`LISTEN_PID`/`LISTEN_FDS`, fd 3)를 지원하며, 이때는
`--listen`이 필요 없다. 넘겨받은 소켓이 Unix인지 TCP인지는 `getsockname`으로
판별한다.

## NixOS 모듈

```nix
{
  inputs.status-api.url = "github:Icicle-Nix/status-api";
  inputs.status-api.inputs.nixpkgs.follows = "nixpkgs";
  # modules = [ inputs.status-api.nixosModules.default ... ];
}
```

```nix
services.status-api = {
  enable = true;
  socket = "/run/status-api.sock";   # 기본값; 경로가 아니면 TCP
  socketGroup = "nginx";             # 기본값 "root"; 소켓은 0660
  disks = [ "/" "/home" "/nix/store" ];
};
services.nginx.virtualHosts.status.locations."= /status.json".proxyPass =
  "http://unix:/run/status-api.sock";
```

모듈은 `enable` 전까지 아무것도 하지 않는다. 소켓 유닛 + `DynamicUser`
서비스로 돌고, 샌드박스는 `nix/module.nix` 참고 (`systemd-analyze security`
1.4). VM 테스트(`nix/test.nix`)가 이 샌드박스 안에서 `systemctl`/`journalctl`이
실제로 동작하는지(sshd `active`, 유도한 `Invalid user`가 집계되는지),
소켓 권한, HTTP 경계 사례, TCP 소켓을 검증한다.

## 보안상 트레이드오프: `CAP_DAC_READ_SEARCH`

RAPL `energy_uj`는 PLATYPUS 사이드 채널 이후 root 전용(0400)이다. 이것 하나를
읽으려고 서비스에 `CAP_DAC_READ_SEARCH`를 준다(Ambient + BoundingSet, 그
외의 capability는 없음). 이 capability는 **모든 파일의 읽기 권한 검사를
우회**하므로, 프로세스가 탈취되면 `ProtectSystem=strict`/`ProtectHome=read-only`
로 읽기 전용이 된 파일 시스템 전체(`/etc/shadow`, 다른 사용자의 홈, 비밀 키
등)를 읽을 수 있다. 쓰기는 여전히 막혀 있고, 네트워크는 `AF_UNIX`만 열 수
있으며 (`IPAddressDeny=any`는 Unix 소켓일 때), 공격 표면은 요청 헤드 파싱
수십 줄뿐이다. 또한 1초 해상도의 패키지 전력은 PLATYPUS가 노리는 정밀도와는
거리가 멀지만, 값 자체를 공개 엔드포인트에 내놓는 셈이므로 소켓은 신뢰하는
리버스 프록시 그룹에만 연다.

CPU 전력이 필요 없다면 `systemd.services.status-api.serviceConfig`에서
`AmbientCapabilities`/`CapabilityBoundingSet`을 비우면 된다 — 그러면
`power.cpu`가 `null`이 될 뿐이다. `ProtectHome=yes`를 쓰지 않는 이유는
`/home`의 `statvfs`가 빈 tmpfs가 아니라 실제 파일 시스템을 봐야 하기
때문이고, `PrivateNetwork`를 쓰지 않는 이유는 `/sys/class/net`에서 호스트
인터페이스가 사라지기 때문이다.

## status.json

이 표가 형식의 기준이다. 모든 값은 요청 시점에 측정된다.

| 필드 | 타입 | 의미 |
|---|---|---|
| `host` | string | `/proc/sys/kernel/hostname` |
| `time` | number | 측정 시각 (epoch 초, 정수) |
| `sshd` | string | `systemctl is-active sshd` 결과; 실행 실패 시 `"unknown"` |
| `ssh24h` | `{ok, reject}` | `journalctl -u sshd --since "-24 hours" -o cat` 줄 수: `ok`는 `Accepted`, `reject`는 `Invalid user`/`Failed`/`not allowed` 포함 줄 (정수) |
| `uptime` | number | 초 (실수, `/proc/uptime`) |
| `load` | number[3] | 1/5/15분 load average |
| `power.cpu` | number \| null | RAPL `intel-rapl:0` 패키지 전력 W (소수 둘째 자리), 1초 구간. AMD에도 같은 경로가 있다. 읽을 수 없으면 `null`. `max_energy_range_uj`로 카운터 순환을 처리 |
| `power.gpu` | number \| null | 첫 번째 활성 DRM 카드의 `hwmon*/power1_average` W; 없으면 `null` |
| `power.gpuAsleep` | bool | 첫 번째 `card<N>`의 `runtime_status`가 `suspended`이면 `true`. 이때 `gpu`는 `null`이고 hwmon은 읽지 않는다 (카드를 깨우지 않기 위해) |
| `mem` | `{used, total, available}` | 바이트, `/proc/meminfo`; `used = MemTotal − MemAvailable` |
| `disks` | `{mount, usedPct, avail}[]` | `--disk`로 준 경로마다 `statvfs`. `mount`는 준 경로 그대로, `avail = bavail·frsize`, `used = (blocks − bfree)·frsize`, `usedPct = ceil(used·100 / (used + avail))` (`df`와 같은 계산). `statvfs`가 실패한 경로는 빠진다 |
| `net` | `{iface, rx, tx, rxRate, txRate}[]` | `lo`를 제외하고 `carrier == 1`인 인터페이스, 이름순. `rx`/`tx`는 부팅 이후 누적 바이트, `rxRate`/`txRate`는 1초 구간의 B/s. 구간 시작에 없던 인터페이스이거나 카운터가 줄었으면 둘 다 `null` |

DRM 카드는 `card` 뒤에 숫자만 붙은 디렉터리(`card0`, `card1`, …)만 본다 —
`card1-DP-1` 같은 커넥터 디렉터리는 `device`가 카드 자신을 가리켜
`runtime_status`가 `unsupported`로 나오기 때문이다.
