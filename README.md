# Sijo (시조) — 게임을 위한 자동 ECS·자동 네트워크 언어

> **C#처럼 쓰고, Rust처럼 안전하며, 컴파일러가 코드를 ECS·병렬·네트워크·분산 형태로 자동 변환하는 게임 전용 언어.**
> Epic의 Verse에서 영감을 받았습니다. "시조(時調)"는 정해진 형식을 지키는 한국의 정형시로, verse(시)라는 이름을 우리 시의 이름으로 이었습니다.

프로그래머는 싱글 플레이어·싱글 스레드·객체지향 코드처럼 작성합니다. 컴파일러가 게임 전체를 **닫힌 세계**로 분석해서 사람이 하던 일을 대신합니다.

| 작성하는 코드 | 컴파일러가 하는 일 |
|---|---|
| `class Ship : Entity { mut Vec2 Pos; ... void Update(float dt) }` | 필드 접근 패턴으로 **컴포넌트 자동 분해**, SoA 테이블, 시스템, **병렬 스케줄** |
| `client void Render() { Draw.Sprite(1, Pos.X, Pos.Y); }` | 여기서 읽는 필드만 **자동 복제**, 범위 기반 **비트 양자화**, 확인된 기준선에 대한 델타를 **실제 UDP**로 |
| `client void Input() { if (...) { Shield(); } }` | 서버 상태를 바꾸는 호출 → **신뢰성 있는 자동 RPC**, 서버가 검증 후 실행 |
| `other.Hp -= 10;` | 결정론적 **지연 명령**(덧셈은 교환 병합, 범위가 있는 필드는 정해진 순서로 명령마다 포화) — 스레드/서버 경계를 넘는 메시지와 동일 |
| `mut var near = new List<Ship>();` | 탈출 분석 → **프레임 아레나** 배치 + 용량 예약, 해제 코드 자동 |
| `mut int in 0..100 Hp` | 범위 타입: 모든 쓰기 지점에 포화 삽입 + **7비트** 직렬화 |
| `Rules.DamageFor(2)` | 순수 함수를 **컴파일 타임에 실행**(CTFE) |
| `atomic { Gold -= p; Items.Remove(x); if (Gold < 0) { fail; } }` | Verse식 트랜잭션 — **정적 실행 취소 로그**로 롤백: 값은 진입 시 저장, 리스트·사전은 처음 바뀌기 직전에만 복사(`Add`만 하면 길이 하나), 명령·Spawn은 버퍼를 자름. 중첩 가능, `return`은 커밋 |
| `async void Run() { ... await Wait(2); ... }` | 코루틴을 상태 기계로 바꾸고 **프레임을 숨은 컴포넌트**로: 병렬·결정론·저장 가능. `await Race(MoveTo(b), Wait(5))`·`Sync(...)`로 구조적 동시성 |
| `foreach (var o in Query<Boid>()) { if ((o.Pos - Pos).LengthSq < R2) ... }` | 거리 조건을 기호적으로 **증명**하고 공간 격자로 — 결과 비트 단위 동일, 24,000개에서 65배 |
| `Name = $"Ship {id}"` / `Dictionary<string, int>` | 문자열은 **프레임 아레나 + 틱 경계 정밀 수집기**: 대입은 포인터 복사, 누수·댕글링 없음 |
| `hp switch { < 0 => ..., 0 => ..., > 0 => ... }` | 구간 분석으로 **완전성·도달 불가 패턴 검사** |
| `ships.Where(s => s.Hp > 0).OrderBy(s => s.Hp)` | 람다를 **호출 지점의 루프로 펼침** — 클로저 할당·간접 호출 없음 |
| `Func<int, int> f = x => x + k;` / `Action<int> heal = ship.Heal;` | 델리게이트: 프로그램의 모든 람다를 알므로 호출은 함수 번호에 따른 **정적 분기**, 효과는 담길 수 있는 함수들의 합집합. 캡처는 값 복사, 파괴된 엔티티를 캡처한 호출은 무시 |
| `static int Describe(int x)` / `static int Describe(Orc? o)` | 메서드 **오버로딩**: C#식 최선 후보 선택, 가상 디스패치·인터페이스 구현도 매개변수 타입으로 |
| `static T Max<T>(T a, T b)` / `struct Pair<T>` / `class Pool<T> : Entity` | 제네릭은 **쓰인 타입 인자마다 따로 검사·컴파일**(인자에서 타입 추론), `where` 제약은 인스턴스마다 검사. 제네릭 엔티티 클래스는 인스턴스(`Pool<int>`)마다 **아키타입·시스템·복제가 따로** |
| `interface IDamageable { void Damage(int n); bool Dead() => Hp() <= 0; }` | 인터페이스 핸들(`IDamageable?`)·`Query<I>()`·`is I`는 구현한 **아키타입마다 정적 디스패치**, 기본 구현은 아키타입마다 컴파일, 제네릭 인터페이스 `IContainer<int>`는 타입 인자마다 인스턴스 |
| `component Burning { ... }` / `ship.Add(new Burning { Left = 5 })` / `Query<Ship, Burning>()` | 런타임에 붙였다 떼는 컴포넌트: 클래스마다 도달 가능한 **조합을 컴파일 타임에 세어 조합마다 SoA 테이블**, 붙이기·떼기는 시스템 끝의 결정론적 구조 변경(행 이동, 핸들 유지), 컴포넌트의 `Update`는 그 테이블들의 시스템. 복제·저장·이주도 그대로 |
| `static extern bool IsKeyDown(EKey k);` | 엔진(C)과 연결, `--lib`로 임베딩 API 생성 |
| `own List<int> Make()` / `void Keep(own List<int> xs)` | 시그니처가 컬렉션의 소유권을 보여 줌: 넘긴 변수는 이동, 이동 뒤 사용은 컴파일 오류 |

## 핵심 아이디어: "단 하나의 규칙"

> 한 시스템 안에서 각 엔티티는 **자기 자신은 즉시** 읽고 쓰고, **다른 엔티티는 시스템 시작 시점의 스냅샷**으로 읽으며, **다른 엔티티에 대한 쓰기·호출·생성·파괴는 지연 명령**이 되어 시스템 끝에 결정론적 순서로 적용된다.

이 규칙 하나로 **모든 시스템이 자동으로 데이터 병렬**이 되고, 결과가 **스레드 수·서버 샤드 수와 무관하게 비트 단위로 같으며**, 엔티티 간 상호작용이 그대로 **분산 가능한 메시지**가 됩니다. 이웃을 읽으며 자신을 쓰는 코드(Boids 등)는 컴파일러가 해저드로 탐지해 해당 컴포넌트만 이중 버퍼링합니다. 자세한 근거는 [`docs/00-overview.md`](docs/00-overview.md).

## 개발 도구: 자동 린터 · 인텔리센스 · 라이브 코딩

- **자동 린터** (`sijoc lint`): Sijo 코딩 표준(이름·서식), .NET 분석기, clang-tidy에서 가져온 규칙과 Sijo 게임 규칙 37개입니다. 게임 규칙의 예는 "여러 엔티티가 같은 필드에 대입하면 마지막 것만 남음", "이 Print 때문에 시스템이 직렬 실행됨", "범위 없는 float 복제는 32비트로 전송됨"입니다. `--fix`는 의미 인덱스를 써서 안전하게 이름을 바꾸고 `var`를 명시 타입으로 바꿉니다. 수정 전후 실행 결과가 같은지 테스트로 확인합니다. 항상 중괄호·선언 하나에 이름 하나·완전한 switch 문·재귀 함수의 `[Recursive]`는 린트가 아니라 문법입니다(컴파일 오류).
- **인텔리센스** (`sijoc lsp`, VS Code 확장 `editors/vscode`): 자동 완성, 정의 이동, 참조 찾기, 이름 변경, 빠른 수정을 지원합니다. 호버와 인레이 힌트에는 컴파일러의 결정이 함께 보입니다. 예: `Hp → Ship_C2 · server-only · [0,100]로 클램프 · Bullet.Update가 deferred ADD`, `Update → stage 1 · parallel`.
- **라이브 코딩** (`sijoc live`): 게임을 실행한 채 코드를 저장하면 약 0.3초 뒤 새 코드로 같은 틱부터 이어서 실행합니다. 상태는 필드 이름 기준으로 옮겨집니다(새 필드는 기본값, 삭제된 필드는 버림). 컴파일 오류가 나면 이전 빌드가 계속 돕니다.

- **여러 파일 프로젝트** (`sijo.toml`): 진단·린트 수정·라이브 코딩·언어 서버(파일 간 정의 이동·이름 변경)가 프로젝트 단위로 동작합니다.
- **디버깅**: 생성 C에 `#line`이 들어가 런타임 오류가 `sijo panic: list index out of range / at units/Ship.sijo:42`처럼 .sijo 줄을 알려 주고, `--debug` 빌드는 gdb에서 .sijo 줄 단위로 디버깅됩니다.
- **엔진 연결**: `sijoc build game --lib`은 `sijo_game.h` 임베딩 API(생성·한 틱·그리기 목록·저장/복원·스레드 수·패닉 복구)를 만듭니다. C·C++·C#(P/Invoke)·Python(ctypes)·Rust·Go 호스트에서 같은 결과로 도는 것을 확인했습니다. `--host host.c`는 호스트 프로그램과 함께, `--externs ext.c`는 sijoc의 `main`(네트워크 모드 포함)을 유지한 채 빌드합니다.
- **검사 모드**: `--checked`는 C#의 `checked`처럼 정수 오버플로·범위 밖 float → int 변환을 .sijo 줄과 함께 패닉으로 알립니다.

자세한 내용: [docs/10-tooling.md](docs/10-tooling.md)

## 빠른 시작

```bash
cd compiler && cargo build --release        # Rust 표준 라이브러리만 사용 (의존성 0)
./target/release/sijoc --version            # sijoc 0.1.0
./target/release/sijoc explain ../examples/arena.sijo   # 컴파일러의 모든 결정과 이유
./target/release/sijoc run ../examples/arena.sijo -- --ticks 600 --render
./target/release/sijoc run ../examples/boids.sijo -- --ticks 300 --shards 4 --timing
cargo test --release                        # 309개 테스트 (진단 106, 파서 19, 이동 13, 범위 11, 인터페이스 6, 컴포넌트 19, 컴파일러 지능 10, 종단 간 48, 임베딩 4, 교차 플랫폼 1, 네트워크 22, 샤드 8, 도구 27, 단위 15; ASan/UBSan/LSan 포함)
./target/release/sijoc lint ../examples/arena.sijo --fix   # 코딩 표준 자동 수정
./target/release/sijoc live ../examples/arena.sijo -- --render-every 60   # 라이브 코딩
./target/release/sijoc lsp                  # 편집기용 언어 서버 (VS Code: editors/vscode)
# 터미널 게임 (여러 파일 프로젝트 + C 호스트, ncurses 필요)
./target/release/sijoc build ../examples/dodge --host ../examples/dodge/host_term.c --ldflags -lncurses --exe /tmp/dodge && /tmp/dodge
```

필요: Rust 1.80+, gcc 또는 clang(MinGW 포함; OpenMP가 있으면 자동 병렬화). 생성된 게임은 Linux·Windows, x86-64·ARM64·빅엔디언에서 돌며 네트워크는 POSIX 소켓과 Winsock을 씁니다. 실행 옵션: `--ticks N --shards N --seed S --render --quiet --timing`, 네트워크 옵션은 아래와 `--help`.

### 어디서 돌려도 같은 결과

같은 프로그램·시드·입력이면 **컴파일러(gcc·clang), 최적화(`-O0`~`-O3 -march=native`), CPU(x86-64·ARM64·빅엔디언 s390x), OS(Linux·Windows), 스레드 수와 무관하게 출력과 월드 체크섬이 비트 단위로 같습니다**. 정수 wrap, 수학 함수(`Sin`~`Pow`), 숫자 ↔ 문자열 변환(.NET 8과 같은 문자열), 해시·직렬화를 런타임이 직접 구현하고, 결과를 바꾸는 FMA 축약·`-ffast-math`·x87을 막습니다. 그래서 서로 다른 플랫폼의 클라이언트가 한 서버에서 함께 플레이하고, 저장 파일을 다른 플랫폼에서 이어서 돌릴 수 있습니다. 검증 방법과 결과: [11장](docs/11-verification.md).

## 네트워크로 실행하기

생성된 실행 파일 하나가 서버도 되고 클라이언트도 됩니다. 옵션을 주지 않으면 두 월드를 한 프로세스에서 실행하며, 이때도 같은 프로토콜 코드를 메모리 링크 위로 돌립니다.

```bash
cd compiler && ./target/release/sijoc build ../examples/arena.sijo --exe /tmp/arena
/tmp/arena --server 7777 --ticks 1800 --wait-clients 2   # 서버: UDP 7777, 클라이언트 2개를 기다렸다가 시작
/tmp/arena --connect 127.0.0.1:7777 --render-every 60    # 클라이언트 (터미널마다 하나씩, 게임 중간에 들어와도 됨)
/tmp/arena --server 7777 --sim-loss 0.2 --sim-latency-ms 30 --sim-jitter-ms 10   # 나쁜 네트워크 시뮬레이션 (클라이언트에도 줄 수 있음)
/tmp/arena --ticks 600 --sim-loss 0.3 --sim-dup 0.1     # 한 프로세스 + 손실 링크 (가상 시간, 결정론적)
```

- 서버는 매 틱 각 클라이언트에게 **그 클라이언트가 확인한 프레임에 대한 델타**를 보냅니다. 손실, 순서 뒤바뀜, 중복을 견디고, 큰 스냅샷은 조각으로 나누며, 손실이 보이면 패리티 조각을 덧붙입니다. 클라이언트 입력(RPC)은 순번과 재전송으로 정확히 한 번 실행됩니다.
- `--ticks N`이 끝나면 서버는 모든 클라이언트가 마지막 틱을 확인할 때까지 기다렸다가 작별 인사를 보냅니다. 양쪽 모두 `replicated state hash: … (tick N, E entities)`와 클라이언트별 대역폭·손실·재전송 통계를 출력합니다. 해시가 같으면 클라이언트 월드가 서버 상태와 비트 단위로 같습니다.
- 복제되는 상태만 읽고 자기 엔티티만 바꾸는 RPC는 클라이언트가 **바로 예측 실행**하고, 스냅샷이 오면 서버 값으로 되돌린 뒤 아직 확인되지 않은 입력만 다시 실행합니다(재조정).
- 클라이언트 코드가 읽는 리스트·배열·사전도 복제됩니다. 그 안의 엔티티 참조(`List<Ship?> Targets`, `Dictionary<Ship?, int>`)는 원소마다 서버 슬롯으로 가서 클라이언트 핸들로 이어지고, 대상이 파괴되었거나 시야 밖이면 null입니다. 큰 리스트가 조금 바뀌면 클라이언트가 확인한 값에 대한 **원소 단위 델타**(바뀐 구간, 덧붙인 꼬리, 새 길이)만 갑니다(512칸 리스트 등: 25,343 → 566 B/tick, [5.5.3절](docs/05-networking.md)).
- 클라이언트 코드가 `Net.View(x, y, r)`로 시야를 정하면 그 클라이언트에게는 시야 안의 엔티티만 보냅니다(관심 영역, [5.8절](docs/05-networking.md)).
- 서버의 `--budget BYTES`는 클라이언트마다 스냅샷 하나를 약 그 바이트 수로 묶습니다. 자리가 없는 변경은 기다리고, 오래 기다린 것·시야 중심에 가까운 것부터 갑니다. 마지막 틱은 전체로 보내 끝에는 서버와 같아집니다(boids 1200마리: 4152 → `--budget 1500` 1543 B/tick).
- `--key HEX`(또는 `SIJO_KEY`)를 서버와 클라이언트에 같이 주면 모든 데이터그램을 XChaCha20-Poly1305로 암호화·인증합니다. 키가 없으면 접속도 패킷 위조도 할 수 없습니다.
- 그 밖의 옵션: `--tick-rate HZ`, `--max-clients N`, `--timeout-ms MS`, `--flush-timeout-ms MS`, `--wait-timeout-ms MS`, `--sim-seed S`. 프로토콜과 측정값은 [5.5절](docs/05-networking.md)에 있습니다.

## 샤드로 나눠 실행하기

같은 실행 파일이 샤드 프로세스도 됩니다. 샤드마다 공간(X 슬랩)을 맡아 자기 엔티티만 계산하고, 스테이지마다 지연 명령·생성·경계 근처 사본(헤일로)·권한 이양을 주고받은 뒤 모든 명령을 같은 순서로 적용합니다. 그래서 **샤드 수와 상관없이 한 프로세스와 출력·월드 체크섬·복제 상태 해시가 비트 단위로 같습니다**.

```bash
/tmp/arena --ticks 600 --shards 4                   # 샤드 월드 4개를 한 프로세스에서
P=127.0.0.1:7001,127.0.0.1:7002,127.0.0.1:7003
/tmp/arena --shard-of 3 --shard-id 1 --peers $P --checkpoint-dir /tmp/ck &   # 샤드 프로세스
/tmp/arena --shard-of 3 --shard-id 2 --peers $P --checkpoint-dir /tmp/ck &
/tmp/arena --shard-of 3 --shard-id 0 --peers $P --checkpoint-dir /tmp/ck --server 7777 --ticks 0   # 코디네이터 = 클라이언트 게이트웨이
/tmp/arena --shard-of 3 --shard-id 2 --peers $P --checkpoint-dir /tmp/ck --recover   # 죽은 샤드를 체크포인트에서 다시
```

`sijoc explain`의 `[shards]`가 시스템마다 실행 방식(분할·복제·코디네이터)과 컴포넌트마다 사본을 보내는 곳(모든 샤드·코디네이터·헤일로 반경)과 그 이유를 보여 줍니다. 자세한 내용: [5.7절](docs/05-networking.md), 옵션: [10.10절](docs/10-tooling.md).

## 벤치마크 (`bench/`)

같은 알고리즘·같은 float 연산 순서로 쓴 구현을 비교합니다: Sijo, 손으로 쓴 데이터 지향 C, 일반적인 객체지향 C++(가상 `Update`, `vector<unique_ptr>`). 같은 설계의 C# 구현(`bench/csharp`)은 .NET 8 SDK가 있으면 함께 잽니다. 모든 실행의 결과가 비트 단위로 같은지 확인합니다. 아래는 sijoc 0.1.0, `-O2`, 틱당 시간입니다(공유 클라우드 VM이라 절대값보다 비율이 의미 있음).

| 워크로드 | Sijo 1스레드 | 손 C 1스레드 | OOP C++ | Sijo 4스레드 | 손 C 4스레드 | Sijo ÷ 손 C (1T) |
|---|---|---|---|---|---|---|
| boids 4000 (O(N²) 이웃) | **3.92 ms** (자동 격자) | 19.7 ms | 20.8 ms | **1.24 ms** | 5.45 ms | **0.20×** |
| particles 100만 (엔티티당 작은 일) | **1.91 ms** | 2.58 ms | 3.75 ms | **0.57 ms** | 0.71 ms | **0.74×** |
| shooter 2000척 (생성·파괴, 원격 피해) | **3.70 ms** | 4.79 ms | 5.77 ms | **1.11 ms** | 3.79 ms | **0.77×** |

- 손으로 쓴 C와 같은 알고리즘일 때 Sijo는 1스레드에서 0.74~0.83배(더 빠름), OOP C++보다 1.5~5배 빠르고, 별도 작업 없이 4스레드에서 2.1~3.4배로 확장됩니다. boids 4000마리에서는 컴파일러가 스스로 증명하고 적용한 공간 격자로 손 C보다 5배 빠릅니다.
- `-O3 -march=native`에서는 두 구현의 particles 루프가 모두 AVX2로 벡터화되며 손 C가 1.0~1.2배 빠릅니다. 메모리(particles 100만)는 Sijo 45MB, 손 C 25MB, OOP C++ 119MB입니다. 자세한 표·방법·분석: [`bench/RESULTS.md`](bench/RESULTS.md). 재현: `bench/run.sh`.

## 예제 (`examples/`)

| 파일 | 보여주는 것 |
|---|---|
| [`arena.sijo`](examples/arena.sijo) | 우주선 전투: 지연 데미지(교환 병합), 해저드 이중 버퍼, 프레임 아레나 리스트, 죽은 필드 제거, 26비트 위치 복제, 클라이언트 입력 → 자동 RPC, CTFE |
| [`boids.sijo`](examples/boids.sijo) | 1200마리 군집: 이웃 읽기 + 자기 쓰기 → 자동 Jacobi + 자동 공간 격자, 스레드 수와 무관하게 체크섬 동일 |
| [`village.sijo`](examples/village.sijo) | 상속·가상 메서드·`base` 호출, 엔티티 간 메시지(`c.Heal(25)`), `atomic` 롤백, 엔티티 소유 리스트 |
| [`dodge/`](examples/dodge) | 플레이 가능한 터미널 게임: 여러 파일 프로젝트, 코루틴 웨이브 감독, `switch` 식, 문자열 HUD, `extern` 키보드·벨, ncurses 호스트(`--headless` 테스트 모드) |
| [`compiler/tests/programs/`](compiler/tests/programs) | 문자열·`char`, 배열·다차원 배열, 중첩 컬렉션, 람다, 제네릭(엔티티 클래스 포함), 인터페이스, 컴포넌트(상태 효과), Dictionary, 코루틴(await 호출 포함), switch, 공간 격자 동등성, 아레나 되감기 등 기능별 실행 테스트 |

`arena.sijo` 실행 결과 일부:

```
tick 121: ship #4 destroyed (kills=4)
...
== sijo runtime summary ==
entities alive: Ship=10 Bullet=20 (spawned 323, destroyed 293)
deferred commands applied: 746
network: 107426 bytes in 601 snapshots (178.7 B/tick), naive full-state: 533.8 B/tick -> 3.0x smaller
client->server RPCs: 47 requests, 188 bytes (executed under server authority)
replication check: OK (30 entities, 70 values compared)
replicated state hash: 2906ffb5530d9159 (tick 600, 30 entities)
frame arena peak: 192 bytes (reset every tick; loops and entities give back their temporaries)
world checksum: 13d8c02780895d46      ← 스레드 1/4, 샤드 월드·샤드 프로세스 1~4 어떤 조합에서도 동일
```

## 문서 (`docs/`, 한국어)

| | |
|---|---|
| [0. 개요와 설계 원칙](docs/00-overview.md) | 왜 새 언어인가, 시스템 규칙, 원칙 |
| [1. 문법](docs/01-syntax.md) | C# 기반 문법, 게임용 확장, 타입·수식어·특성의 원칙(범위 타입, `mut`, `own`, `client`, `extern`), 인터페이스, 런타임 컴포넌트 |
| [2. 타입과 안전성](docs/02-types-and-safety.md) | null 안전 핸들, 2급 참조, 배타성, 기본 불변, 이동 추론과 `own`, 컨텍스트 |
| [3. 메모리](docs/03-memory.md) | 영역(Stack/Frame/Entity/World), 탈출 분석, 용량 추론 |
| [4. 자동 ECS](docs/04-auto-ecs.md) | 접근 서명 클러스터링, 런타임 컴포넌트 조합 테이블, 해저드, 스테이지 스케줄링, 한 틱의 정확한 실행 순서 |
| [5. 네트워크와 분산](docs/05-networking.md) | 복제 추론, 범위 양자화, 확인된 기준선 델타, 실제 UDP 전송, 샤드 프로세스(전역 순서 키, 헤일로, 이양, 복구) |
| [6. 효과·트랜잭션·동시성](docs/06-effects-and-concurrency.md) | 효과 추론, `atomic`, 코루틴과 구조적 동시성 |
| [7. 컴파일러 구조](docs/07-compiler.md) | 파이프라인, 최적화 패스, `explain` |
| [8. 구현 현황과 로드맵](docs/08-roadmap.md) | 무엇이 동작하고 무엇이 계획인지 |
| [9. 참고 자료](docs/09-references.md) | Verse, Perceus, Lobster, Vale, Hylo, Iris, SpatialOS 등 |
| [10. 개발 도구](docs/10-tooling.md) | 자동 린터, 인텔리센스(LSP), 라이브 코딩, 엔진 임베딩, 결정론·지원 플랫폼 |
| [11. 검증 보고서](docs/11-verification.md) | 플랫폼 행렬, 다른 언어 구현과의 비교, 성능, 퍼징, C#과 다른 점 |

## 저장소 구조

```
compiler/src/   lexer → parser → sema(타입 검사, HIR, 의미 인덱스, switch·코루틴 하강) → opt(접기/CTFE/BCE)
                → components(컴포넌트 조합 → 아키타입) → effects(효과 추론) → safety(배타성·이동·확정 대입) → ecs(컴포넌트·스케줄) → memory(영역)
                → net(복제·양자화) → spatial(근접 질의 증명) → shard(샤드 계획) → codegen(C 생성, #line)
                / explain(보고서) / lint(린터) / lsp(언어 서버) / live(라이브 코딩) / driver(CLI, 프로젝트)
editors/vscode/              VS Code 확장 (LSP 클라이언트, 문법 강조, 라이브 코딩 명령)
compiler/runtime/sijo_rt.h   생성 코드에 포함되는 런타임 (핸들, 아레나, 명령 버퍼, 비트 패킹, RNG, 복제 프레임)
compiler/runtime/sijo_str.h  문자열 + 틱 경계 수집기       sijo_dict.h  Dictionary       sijo_grid.h  근접 질의 격자
compiler/runtime/sijo_net.h  네트워크 런타임 (UDP, 네트워크 시뮬레이션, 델타 코덱, 조각화, 신뢰성 있는 RPC, 실행 방식)
compiler/runtime/sijo_shard.h, sijo_shard_net.h  샤드 런타임 (교환·병합·헤일로·이양·재조정 / 신뢰성 채널·체크포인트·복구)
compiler/runtime/sijo_game.h 임베딩 API (--lib)
compiler/tests/              진단 테스트, 종단 간 테스트, 네트워크(다중 프로세스) 테스트, 의미론 프로그램
examples/                    예제 게임
docs/                        설계 명세
```

## 상태

**버전 0.1.0** — Sijo의 첫 버전이며, 실제로 게임을 만들 수 있는 단계를 목표로 하는 **프로토타입**입니다. 언어(문자열·`char`·배열·중첩 컬렉션·람다·제네릭·인터페이스·런타임 컴포넌트·사전·코루틴·switch), 자동 ECS·병렬화·공간 격자, 엔진 연결, 서버 ↔ 클라이언트 복제(엔티티 참조·문자열·리스트·사전, 컬렉션 안의 참조와 큰 리스트의 원소 단위 델타, IPv4/IPv6 실제 UDP), 한 프로세스와 같은 결과를 내는 샤드 프로세스가 동작합니다. 모든 기능은 실행 테스트(ASan/UBSan, 스레드 수 무관 체크섬, 저장·복원 동일성)로 확인하고, 플랫폼 행렬·다른 언어 구현과의 비교·퍼징 결과는 [11장](docs/11-verification.md)에 있습니다. 렌더 보간·플레이어별 인증·큰 스냅샷의 손실·코디네이터 복구 등 남은 한계는 [로드맵](docs/08-roadmap.md)과 [11.6절](docs/11-verification.md)에 정리했습니다. 버전별 변경은 [CHANGELOG.md](CHANGELOG.md)에 적습니다.

## 라이선스

[MIT](LICENSE). `compiler/`(크레이트)와 `editors/vscode/`(확장)는 따로 배포되므로 같은 LICENSE 사본을 함께 둡니다.
