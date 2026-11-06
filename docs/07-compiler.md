# 7. 컴파일러 구조와 최적화

## 7.1 전체 구조

```
.sijo ─▶ Lexer ─▶ Parser ─▶ AST
                              │
                              ▼
                 Sema: 이름 해석 · 타입 검사 · 문맥 리터럴 타이핑
                              │
                              ▼  HIR (타입이 붙은 트리, 모든 이름이 ID로 해석됨)
        ┌─────────────────────┼──────────────────────────────┐
        ▼                     ▼                              ▼
  안전성 검사           효과/접근 분석 (호출 그래프 고정점)     최적화 I (HIR)
  null·불변·배타성·       │                                  상수 접기/전파, CTFE,
  이동·컨텍스트·재귀      ▼                                  e-graph·값 범위, 분기 제거
                  컴포넌트 조합 ─ 클래스마다 도달 가능한 조합 = 아키타입 (4.4.1절)
                  ECS 계획 ─ 컴포넌트·아키타입·시스템·해저드
                  스케줄 계획 ─ 스테이지·병렬 판정
                  메모리 계획 ─ 탈출 분석·영역·용량
                  네트워크 계획 ─ 복제·범위·비트 수
                  공간 계획 ─ 근접 루프 증명 → 해시 격자
                  샤드 계획 ─ 시스템 실행 방식·컴포넌트 사본 경로·헤일로 반경 (src/shard.rs, 5.7절)
                              │
                              ▼
                    최적화 II (계획 기반)
                    죽은 필드 제거, 경계 검사 제거,
                    쿼리 단형화, 값/포인터 전달 선택
                              │
                              ▼
               백엔드: C11 생성 (#line으로 .sijo 줄 연결) ─▶ gcc/clang -O2 (+OpenMP)
                              │
                              ▼
                    네이티브 실행 파일 + explain 보고서
```

## 7.2 왜 "언어 수준 최적화 + C/LLVM 위임"인가

LLVM/GCC는 레지스터 할당, 명령어 선택, 루프 벡터화를 이미 잘합니다. 그러나 다음은 **C 수준에서는 알 수 없는 정보**라서 LLVM이 할 수 없습니다.

| 언어 수준에서만 가능한 최적화 | 근거가 되는 언어 정보 |
|---|---|
| 데이터 레이아웃 변경 (AoS→SoA, 컴포넌트 분할, 죽은 필드 제거) | 닫힌 세계 + 모든 필드 접근을 앎 |
| 자동 병렬화 | 시스템 규칙 (엔티티 간 의존 없음이 *의미론적으로* 보장) |
| 별칭 없음 보장 (`restrict`, 계획) | 배타성 규칙 — 현재는 생성 C가 타입 기반 별칭 규칙을 지키므로 `-fno-strict-aliasing` 없이 컴파일 |
| 가상 호출 제거 | 아키타입별 단형화 |
| 네트워크 비트 수 결정 | 범위 타입(`int in 0..100`) + 범위 추론 |
| 할당 제거/영역 배치 | 탈출 분석 + 2급 참조 |
| 컴파일 타임 평가 | 효과 추론으로 순수성 증명 |

Mojo(MLIR), Unity Burst(LLVM), Jai의 접근과 같은 분업입니다. Sijo 컴파일러는 *의미*를 최적화하고, 기계어는 성숙한 백엔드에 맡깁니다.

## 7.3 최적화 패스 목록

| 패스 | 설명 |
|---|---|
| 상수 접기 / 전파 | 정수·실수·불리언 연산, 한 번만 대입되는 지역 상수 전파 |
| CTFE | 순수 함수가 상수 인자로 호출되면 컴파일 타임에 인터프리터로 실행해 결과로 치환 (연료 제한) |
| e-graph 동등성 포화 | 정수·불리언 식을 e-graph(egg 방식)에 넣고 재작성 규칙 80여 개(교환·결합·분배, 항등원, 비트 연산, 비교 정규화, 단락 평가 `&&`/`||`, 삼항)를 더 이상 새 형태가 생기지 않을 때까지 적용한 뒤 비용이 가장 낮은 식을 꺼냄. 정수 연산은 C#처럼 wrap하므로(가환환) 재결합이 비트 단위로 정확하고, `--checked`에서는 오버플로가 트랩이므로 환 규칙을 끄고 구간이 오버플로 없음을 증명할 때만 상수로 접음. 피연산자를 버리는 규칙(`x*0`, `x-x`)은 그 피연산자가 패닉할 수 없을 때만, 순서를 바꾸는 규칙은 부수 효과가 없고 한쪽이 패닉할 수 없을 때만 적용(첫 패닉과 그 메시지가 그대로). 실수는 IEEE 정확한 항등식(`x*1`, `x/1`, `x-0`)만 (`src/egraph.rs`, `--no-egraph`로 끄고 비교) |
| 값 범위 | e-graph의 e-클래스 분석으로 구간을 추적. 근거: 범위 타입(모든 쓰기가 포화하므로 읽은 값은 범위 안), 불변 지역 변수(초기화 식의 구간, 불변 지역 변수만 읽는 정의는 펼쳐서 `a + K + 1 - a`를 상수로), `for` 카운터(시작값 ≤ i < 상한), 배열 길이(`new T[N]`은 길이가 바뀌지 않음; 다차원 배열 지역 변수 `new T[W, H]`는 원소를 써도 통째로 바꾸지 않는 한 차원마다 길이가 그대로), 컬렉션 길이 ≥ 0. 쓰임: 비교·조건 결정(`if (Hp > 100)` 제거), 포화 제거 또는 한쪽만 남기기(`Hp -= 7` → 아래 끝만), 0 이상인 값의 `% 2^k`·`/ 2^k` → 마스크·시프트, `Math.Abs/Min/Max/Clamp` 결정, 인덱스 구간이 배열 안이면 경계 검사 제거. 결과가 정확하려면 계약이 모든 입구에서 지켜져야 하므로 필드 기본값·상태 불러오기·클라이언트 복원·RPC 인자도 포화함 |
| 분기 제거 | 조건이 상수인 `if`/`while(false)`/삼항 |
| 죽은 필드 제거 | 어떤 코드도 읽지 않는 엔티티 필드를 저장소·복제에서 제거. 소스에 적힌 대로의 읽기로 판단하므로(최적화가 읽기를 없애도 저장소는 남음) 저장되는 월드와 체크섬은 최적화의 강도와 무관 |
| 경계 검사 제거 | `for (i = 0; i < xs.Count; i++)`에서 `xs`가 루프 내 불변이면 `xs[i]` 검사 생략. 다차원 배열은 첨자마다: `for (x = 0; x < g.GetLength(0); x++)` 안의 `g[x, ..]`는 0번 차원 검사만 생략(다른 차원의 첨자는 따로 증명) |
| 쿼리 단형화 | `Query<T>()` 루프를 아키타입별로 복제, 루프 변수를 커서로 |
| 테이블 사이의 메서드 공유 | 한 클래스의 조합 테이블들, 한 컴포넌트가 있는 테이블들에서 메서드를 한 번만 컴파일. 자기 행은 뷰(`SijoView_N`: 열 포인터와 아키타입 번호, 시스템 루프마다 한 번)로 닿음. 16조합을 넘는 클래스는 컴포넌트 비트를 가진 열린 테이블 하나(4.4.1절) |
| 샤드 계획 | 시스템마다 분할·복제·코디네이터 실행, 컴포넌트마다 다른 샤드가 읽는 곳(모든 샤드·코디네이터·증명된 반경의 헤일로), 스테이지를 생성 합치기로 나눌 곳. 정확한 컴포넌트 코덱과 주인 행 필터를 생성(5.7절, `explain`의 `[shards]`) |
| 정적 디스패치 | 상속 메서드를 아키타입별로 해석 |
| 값/포인터 전달 선택 | 16바이트 이하 struct는 값, 큰 것은 `const*` (의미 동일) |
| 엄격한 별칭 규칙 | 생성 C는 타입 퍼닝을 `memcpy`/공용체로만 하므로 `-fno-strict-aliasing` 없이 컴파일(필드가 많은 루프에서 최대 3배). `restrict` 부착은 아직 계획 |
| 스레드별 문맥 | 스레드 id는 스레드당 한 번, 엔티티 문맥(명령 출처·난수 흐름)은 그 시스템이 쓸 때만, 캐시 라인 단위로 패딩 |
| 복제 없는 프로그램 | 아무것도 복제하지 않으면 틱마다 슬롯을 훑는 캡처 단계를 만들지 않음 |
| 무할당 포맷팅 | `Print($"...")` → printf 직행 |
| 용량 예약 | 3장 |
| switch 하강 | 조건 체인 + `switch(0)` 래퍼(C#의 break/continue 의미 그대로), 상수로 접힌 갈래 제거 |
| 코루틴 하강 | async 메서드 → 상태 기계, await를 넘는 지역 변수 → 숨은 필드, `await M(args)` → 기다리는 쪽이 매 틱 이어서 실행 (4.11절) |
| 제네릭 인스턴스화 | 제네릭 메서드·struct·엔티티 클래스를 쓰인 타입 인자마다 선언·검사(C++ 템플릿식, 인자에서 타입 추론). 엔티티 클래스의 인스턴스는 그대로 엔티티 클래스가 되어 뒤 단계(ECS·네트워크·코드 생성)는 제네릭을 모름. 첫 인스턴스만 IDE 색인에 넣고, 다른 인스턴스의 멤버를 가리키는 참조는 첫 인스턴스의 멤버로 기록(이름 바꾸기가 모든 사용처에 적용) (1.14절) |
| 람다 인라인 | 리스트·배열 메서드의 람다를 호출 지점의 루프로 펼침(클로저 할당·간접 호출 없음), `Sort(비교)`는 안정 병합 정렬 |
| 행 루프 | 엔티티 시스템의 행 루프를 `setjmp` 영역 밖의 별도 함수로 두고, 행의 필드를 지역 변수에 읽어 두었다가 한 번 씀 → 자기 쓰기만 하는 시스템이 SIMD로 벡터화 |
| 핸들 검사 끌어올리기 | 안쪽 쿼리 루프에서 바뀌지 않는 핸들의 생존 검사·행 조회를 루프 밖으로 |
| 해저드 복사 축소 | 이중 버퍼는 다른 엔티티가 읽는 필드만, 스레드들이 나눠 복사 |
| 아레나 되감기 | 반복·엔티티 단위로 프레임 아레나를 되돌림 (3.3.1절) |
| 병렬 Spawn | 임시 핸들(스레드·행)로 병렬 생성 후 행 순서로 합치며 진짜 핸들로 고침 (4.8절) |
| 시스템 간 태스크 병렬 | 같은 스테이지의 작은 시스템들을 OpenMP 태스크로 나란히, 구성원별 명령 버퍼를 스테이지 순서로 병합 (4.8절) |
| PGO | `--profile FILE` 실행이 남긴 시스템별 비용으로 병렬 임계값·태스크 그룹·핫 필드 분리를 결정, `--profile-fields`로 잰 필드별 접근 횟수로 레코드 나누기, 결과 동일 (4.8절) |
| GPU 오프로딩 | `sijoc build --gpu`: GPU 후보(`sijoc explain`의 `GPU offload candidates`: 자기 행의 평범한 데이터 필드만 읽고 쓰며 명령·Spawn·파괴·메시지·쿼리·다른 엔티티 읽기·난수·Print·할당·코루틴·extern 호출이 없는 시스템)의 행 루프를 `#pragma omp target teams distribute parallel for`로. 월드 구조체는 `map(to: w[0:1])`, 읽기만 하는 열은 `map(to:)`, 쓰는 열은 `map(tofrom:)`(`w->Particle.c0[0:n]`), 그 루프가 부르는 메서드 인스턴스·정적 함수·struct 메서드는 `#pragma omp declare target`. 장치 컴파일러에도 `-foffload-options=-ffp-contract=off`(GCC 오프로드 컴파일러가 받지 않는 `-fcf-protection`은 끔)를 주고 sijo의 수학 함수가 함께 장치로 감. `SIJO_GPU_MIN`(기본 4096)행 이상인 테이블만 보내고, 샤드에서는 자기 행만 도는 호스트 루프. 장치가 없으면 OpenMP가 호스트에서 실행. GCC 13 + nvptx로 PTX 장치 코드까지 만들어지고, 호스트 대체 실행 결과가 보통 빌드와 비트 단위로 같음(`tests/intelligence.rs`). 실제 GPU에서는 아직 확인하지 않아 기본은 꺼짐 |
| 근접 질의 가속 | 거리 조건을 기호 계산으로 증명한 루프 → 해시 격자, 결과 비트 단위 동일 (4.12절) |
| 문자열 루트 생성 | 타입에서 문자열 칸을 찾아 틱 경계 수집기의 정밀 루트 방문 함수 생성 (3.8절) |

## 7.4 향후 패스 (로드맵)

- **e-graph와 값 범위**(7.3절)에 더할 것: 실수의 정확한 규칙(2의 거듭제곱 역수 곱 등), 경로 민감 사실(`if (i < xs.Count)` 안의 `xs[i]`), 순수 함수 호출을 원자로 공유해 공통 부분식 제거.
- **PGO**(4.8절)에 더할 것: 실행 중 프로파일을 모아 다음 실행(라이브 코딩 재시작)에 반영.
- **GPU 오프로딩**(7.3절)에 더할 것: 실제 GPU에서 같은 비트가 나오는지 확인(그때까지 `--gpu`는 기본으로 꺼짐), 장치에 열을 계속 두고 다른 시스템이 읽기 전에만 옮기기(`target enter/exit data`), 월드 전체 대신 쓰는 테이블만 보내기. 어느 시스템을 장치에서 돌릴지는 지금처럼 컴파일 때의 판정과 행 수로만 정함.
- **개발 모드 백엔드**: Cranelift/인터프리터로 즉시 컴파일 + 핫 리로드 (컴포넌트 레이아웃이 바뀌면 상태 마이그레이션 코드 자동 생성).

## 7.5 `sijoc explain`

모든 계획 결정을 근거와 함께 출력합니다. `examples/arena.sijo`의 실제 출력 일부:

```
[optimizer]
  constants folded: 11, ..., compile-time calls (CTFE): 1
  ctfe: Rules.DamageFor(...) = 11
  const HitRadiusSq = 2.25
[systems] server tick = 3 stage(s)
  stage 0:
    Ship.Update over [Ship]  -- serial: spawns entities ...; prints (output order)
      hazard: Ship.Pos written on self and read on other entities -> double-buffered [Ship_C0]
  stage 1:
    Bullet.Update over [Bullet]  -- parallel (data-parallel over entities)
      effects: ... defers writes {Ship.Hp (+=), Ship.Kills (+=)}; queries {Ship}; destroys self
      after Ship.Update: writes Ship:Ship_C0
[deferred]
  Bullet.Update: writes Ship.Hp on other entities -> deferred ADD, saturated into 0..100 after each command, applied in a fixed order (sender row order): deterministic, but the order matters
  Bullet.Update: writes Ship.Kills on other entities -> deferred ADD (commutative: concurrent writes merge exactly)
[delegates] a call through a value runs the function it holds (chosen by number, statically dispatched; effects = all of them)
  (델리게이트를 쓰는 프로그램: 타입마다 담길 수 있는 람다·메서드와 캡처, 예: `lambda Ship.Update__lambda2 (line 32): captures this: Ship, call skipped once it is destroyed`)
[memory]
  line 89: List<Ship> in Ship.Steer -> Frame arena (...), reserve Count<Ship>() (no reallocation)
[network]
  replicate Ship.Pos (26 bits): Pos.X: 13 bits (step 0.01) [declared range 0..64, precision 0.01]; ...
  server-only (never sent, client code does not read them): Ship.Vel, Ship.Hp, Ship.Cooldown, ...
  Ship full state: 27 bits vs 96 bits unquantized (3.6x smaller before delta compression)
[shards] separate shard processes (--shard-of N --shard-id I --peers ...) or worlds (--shards N)
  Ship.Update: partitioned — each shard runs the entities it owns
  Game.Update: coordinator — only reads and prints: runs on the coordinator
  Ship_C0 {Pos} -> every shard: Ship.Update reads it on other entities through handles or a scan without a proven radius; ...
  Ship_C4 {Hp, Kills} -> every shard: message Ship.Shield (applied on every shard) reads it
  no halo for the proximity loop over Ship in Bullet.Update: Bullet.Update may move this entity before the loop runs
  barriers per tick: 4 (RPCs at the start) (one per stage)
```

## 7.6 구현 언어와 크기

프로토타입 컴파일러는 Rust(표준 라이브러리만, 외부 의존성 0, 약 3.7만 줄; C 런타임 헤더 약 7,600줄)로 작성되었습니다. 런타임은 C 헤더로 생성 코드에 포함됩니다. `sijo_rt.h`는 맨 앞에 들어가는 핵심 런타임(핸들, 아레나, 명령 버퍼, 비트 패킹, 프레임 자료 구조), `sijo_str.h`는 문자열과 틱 경계 수집기, `sijo_dict.h`는 Dictionary, `sijo_grid.h`는 근접 질의 격자, `sijo_net.h`는 네트워크 런타임(UDP 전송, 네트워크 조건 시뮬레이션, 델타 코덱, 조각화, 신뢰성 있는 RPC, 실행 방식), `sijo_crypto.h`는 XChaCha20-Poly1305, `sijo_shard.h`와 `sijo_shard_net.h`는 샤드 런타임(교환·병합·헤일로·이양 / 샤드 사이 채널·체크포인트·복구), `sijo_embed.c.h`와 `sijo_game.h`는 `--lib` 임베딩 API입니다. `sijo_net.h`는 사용자 식별자를 쓰는 생성 코드가 모두 끝난 뒤에 들어갑니다. 소켓 헤더가 정의하는 소문자 매크로(`h_addr` 등)가 필드 이름을 바꿔 버리지 않게 하기 위해서입니다.
