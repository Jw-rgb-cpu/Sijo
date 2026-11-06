# 9. 조사 자료와 출처

설계 전에 조사한 자료입니다. 각 항목 옆에 Sijo 설계에 어떻게 반영했는지 적었습니다.

## Verse / Epic Games
- Epic, *Failure in Verse* — 실패 가능 식, 실패 컨텍스트, `<transacts>` 요구 — https://dev.epicgames.com/documentation/en-us/uefn/failure-in-verse → `atomic`/`fail` (6.2절)
- Augustsson, Breitner, Claessen, Jhala, Peyton Jones, Shivers, Steele, Sweeney. *The Verse Calculus: A Core Calculus for Deterministic Functional Logic Programming*, ICFP 2023 (PACMPL, 오픈 액세스) — https://doi.org/10.1145/3607845 → 결정론을 언어 의미론으로 보장한다는 방향
- Epic, *The road to Unreal Engine 6* — UE6를 Verse 위에 구축, 엔진 시스템을 Verse API가 있는 공개 사양으로 — https://www.unrealengine.com/news/the-road-to-ue-6
- Inven Global, UE6에서 Actor/Blueprint 제거, Scene Graph(엔티티+컴포넌트)로 이동 (Unreal Fest Tokyo 2026-11-03) — https://www.invenglobal.com/articles/28098/epic-games-will-remove-blueprints-and-actors-from-unreal-engine-6 → Sijo의 엔티티/컴포넌트 모델이 같은 방향임을 확인
- Inven Global, Sweeney: 트랜잭션 메모리를 대규모 시뮬레이션 확장의 핵심으로 — https://www.invenglobal.com/articles/22928/ceo-tim-sweeney-unreal-engine-6-will-fundamentally-change-the-development-paradigm → "시스템 규칙"(스냅샷 격리 + 교환 병합)을 더 싼 대안으로 채택
- Epic, *Using persistable data in Verse* — https://dev.epicgames.com/documentation/ko-kr/fortnite/using-persistable-data-in-verse → `[Persistent]` 필드(5.7.11절)

## 메모리 관리
- Reinking, Xie, de Moura, Leijen. *Perceus: Garbage Free Reference Counting with Reuse*, PLDI 2021 — https://www.microsoft.com/en-us/research/publication/perceus-garbage-free-reference-counting-with-reuse/ → 정적 소유권 추론으로 RC 연산 제거 아이디어
- Lobster, *Memory Management* (compile-time reference counting, RC 연산 ~95% 제거) — https://aardappel.github.io/lobster/memory_management.html → 값마다 소유권 분석
- Vale, *Generational References* — https://vale.dev/blog/generational-references → 엔티티 세대 핸들
- Racordon & Abrahams, *Borrow checking Hylo* (IWACO 2023) — https://2023.splashcon.org/details/iwaco-2023-papers/5/Borrow-checking-Hylo → 가변 값 의미론 + 2급 참조, 소유권을 넘기는 `sink` 매개변수 → `own`
- Mojo Manual, *Death of a value* (ASAP destruction) — https://docs.modular.com/mojo/manual/lifecycle/death → 마지막 사용 기반 해제
- Tofte & Talpin, 영역 추론과 MLKit 회고 — https://elsman.com/mlkit/pdf/retro.pdf → Frame/Entity/World 영역과 영역 다형 반환
- Microsoft, *C# 11 Low Level Struct Improvements* (ref safety, `scoped`) — https://learn.microsoft.com/en-us/dotnet/csharp/language-reference/proposals/csharp-11.0/low-level-struct-improvements → `ref`/`out` 2급 참조 규칙의 C# 친화적 표현

## ECS / 병렬화 / 데이터 레이아웃
- Bevy, *nondeterministic_system_order* 예제와 스케줄러 모호성 탐지 — https://docs.rs/bevy_ecs/latest/src/bevy_ecs/schedule/schedule.rs.html → Sijo는 소스 순서를 기본값으로 삼아 모호성 자체를 제거
- Unity, *Enhanced Aliasing with Burst* / Burst aliasing guide — https://unity.com/blog/technology/enhanced-aliasing-with-burst , https://docs.unity3d.com/Packages/com.unity.burst@1.6/manual/docs/OptimizationGuidelines-Aliasing.html → 배타성 규칙으로 `restrict`를 *보장*
- Radtke & Weinzierl, *Compiler support for semi-manual AoS-to-SoA conversions with data views* (2024) — https://arxiv.org/abs/2405.12507 → 자동 AoS→SoA 판단 휴리스틱 부재 문제 → 접근 서명 클러스터링으로 해결 시도
- 구조체 분할/필드 재배치/죽은 필드 제거 — https://web.eecs.umich.edu/~mahlke/courses/583f23/lectures/Dec4/group21_paper.pdf

## 네트워크 / 분산
- Epic, *Introduction to Iris* / *Iris prioritization* — https://dev.epicgames.com/documentation/en-us/unreal-engine/introduction-to-iris-in-unreal-engine → 양자화된 상태 사본, 폴링 대신 변경 통지(컴파일러 삽입 더티 플래그), 우선순위
- Unity, *Netcode for Entities: Data compression / Ghost snapshots* — https://docs.unity3d.com/Packages/com.unity.netcode@1.3/manual/compression.html → 기준선 델타, 수동 양자화 주석 → 범위 기반 자동 비트 수
- NATO MSG-175 프라이머(SpatialOS 소개) — https://www.sto.nato.int/publications/STO%20Meeting%20Proceedings/STO-MP-MSG-175/MP-MSG-175-01P.pdf → 공간 분할로 서버 부하를 나눈다는 문제 설정. Sijo의 샤드 분할·이양·재조정(5.7.5절)은 X축 슬랩, 히스테리시스, 가중 분위수 경계를 쓰는 독자 설계
- 롤백/예측 넷코드 개요 — https://crux.supercraft.host/blog/rollback-netcode-explained/ → 결정론적 시뮬레이션 + 고정 틱
- Fabien Sanglard, *Quake 3 Source Code Review: Network Model* — https://fabiensanglard.net/quake3/network.php → 클라이언트가 확인한 스냅샷에 대한 델타, 재전송 큐 없이 최신 상태로 손실 복구 (5.5.2)
- Glenn Fiedler, *Snapshot Compression*, *Packet Fragmentation and Reassembly*, *Reliable Ordered Messages* — https://gafferongames.com/post/snapshot_compression/ , https://gafferongames.com/post/packet_fragmentation_and_reassembly/ , https://gafferongames.com/post/reliable_ordered_messages/ → 확인된 기준선 델타, 조각화, 순번·재전송 기반 신뢰성 있는 RPC
- netcode (구 netcode.io) 표준 — https://github.com/networkprotocol/netcode → 데이터그램 봉인(헤더는 인증된 평문)과 새 연결의 주소 확인(5.5.3)

## 컴파일러 최적화
- Willsey et al., *egg: Fast and Extensible Equality Saturation*, POPL 2021 — https://arxiv.org/abs/2004.03082 → e-graph 동등성 포화와 값 범위 분석(7.3절)

## 개발 도구 (10장)
- 김포프(POCU), *C# 코딩 표준* — https://github.com/popekim/dev-docs-ko/blob/master/coding-standards/csharp.md → Sijo 코딩 표준(10.1절)의 바탕: 린터 이름 규칙 L101–L114(파스칼·카멜 표기, `S`/`E`/`I`/`b`/`mb`/`m` 접두사, ALL_CAPS 상수, bool의 `Is`/`Can`/`Has`/`Should`, 약어 표기), 서식 L202–L205(`var` 제한, Allman 중괄호, 멤버 순서). 규칙은 Sijo에 맞게 다시 정리했고 예시는 Sijo 코드로 바꿨음. 그 표준의 항상 중괄호·한 줄에 선언 하나·switch의 `default:`·이름 접미사 `OrNull`/`Recursive`는 Sijo에서 문법(E0031, E0030, E0237, `[Recursive]`)이거나 타입(`T?`)이 말함
- Microsoft, *.NET code-style rules* (IDE0011, IDE0059, IDE0060, IDE1006 ...) — https://learn.microsoft.com/en-us/dotnet/fundamentals/code-analysis/style-rules → `dotnet` 프리셋과 정확성 규칙
- LLVM *clang-tidy* checks (`bugprone-integer-division`, `bugprone-branch-clone`, `readability-braces-around-statements`, `readability-function-cognitive-complexity` ...) → L305, L309, L313, L314, 그리고 문법 규칙 E0031(항상 블록)
- Microsoft, *Language Server Protocol Specification* — https://microsoft.github.io/language-server-protocol/ → `sijoc lsp`
