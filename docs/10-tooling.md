# 10. 개발 도구 — 자동 린터, 인텔리센스, 라이브 코딩

Sijo의 도구는 모두 **컴파일러와 같은 분석 결과**를 씁니다. 그래서 일반 린터·IDE가 알 수 없는 정보(이 필드가 몇 비트로 복제되는지, 이 시스템이 왜 직렬로 도는지, 이 리스트가 어느 메모리 영역에 있는지)를 편집기에서 바로 보여줄 수 있습니다.

| 도구 | 명령 | 내용 |
|---|---|---|
| 린터 | `sijoc lint file.sijo [--fix] [--preset standard\|game\|dotnet]` | 38개 규칙, 의미 기반 자동 수정 |
| 인텔리센스 | `sijoc lsp` (LSP 서버) + `editors/vscode` | 진단·완성·호버·정의·참조·이름 변경·빠른 수정·인레이 힌트 |
| 라이브 코딩 | `sijoc live file.sijo` | 저장하면 게임이 멈추지 않고 새 코드로 이어서 실행 |
| 프로젝트 | `sijoc <명령> <디렉터리>` (+ `sijo.toml`) | 여러 파일을 한 프로그램으로 (10.5절) |
| 디버깅 | `sijoc build --debug` | `#line`으로 gdb·새니타이저·런타임 패닉이 .sijo 줄을 가리킴 (10.6절) |
| 엔진 연결 | `sijoc build --lib` / `--host host.c` | 임베딩 C API, 필드 읽기 함수, `extern` 호스트 함수 (10.7절) |
| 웹 | `sijoc build --target wasm32` | WebAssembly 모듈 + JS 로더 + 웹 페이지 (10.11절) |
| 분산 실행 | `prog --shards N` / `prog --shard-of N --shard-id I --peers ...` | 샤드 월드·샤드 프로세스, 체크포인트·복구 (10.10절) |

## 10.1 자동 린터

"자동"의 의미는 세 가지입니다. 첫째, `sijoc check`와 편집기는 매번(편집기는 매 입력마다) 린트를 실행합니다. `build`/`run`은 요약과 함께 **게임 동작·속도에 관한 것(L4xx)과 정확성 경고를 먼저** 한 줄씩 다섯 개까지 보여 주고, 이름·모양 힌트는 개수로만 셉니다(`lint: 2 warning(s), 381 hint(s) (3 about how the game runs, 352 about names and layout)`). 둘째, 고칠 수 있는 항목은 `--fix`나 편집기의 *Fix All*이 **의미를 보존한 채** 자동으로 고칩니다. 셋째, 게임 전용 규칙은 사람이 따로 지정하지 않아도 컴파일러가 이미 계산한 계획(ECS, 스케줄, 네트워크, 메모리)에서 바로 나옵니다.

### 규칙 출처

| 계열 | 코드 | 근거 |
|---|---|---|
| 이름 | L101–L114 | Sijo 코딩 표준(아래), .NET 이름 규칙 IDE1006 |
| 스타일 | L202–L205 | Sijo 코딩 표준(Allman 중괄호, 명시 타입, 멤버 순서) / .NET IDE0008·SA1201·`csharp_new_line_before_open_brace` |
| 정확성 | L301–L316 | C# 컴파일러·분석기(CS0162, CS0168, CS0219, CS1717, IDE0051, IDE0059, IDE0060, IDE0100, CA1508), clang-tidy(`bugprone-integer-division`, `bugprone-branch-clone`, `readability-simplify-boolean-expr`, `readability-function-cognitive-complexity`), clang `-Wfloat-equal`·`-Wself-assign`, rustc `unused_mut` |
| 게임 (Sijo 전용) | L401–L408 | 시스템 규칙, 스케줄러, 네트워크·메모리 계획기 |

전체 목록은 `sijoc lint --list`로 봅니다.

### Sijo 코딩 표준 (`standard` 프리셋, 기본)

김포프(POCU)의 C# 코딩 표준(9장)을 바탕으로 Sijo에 맞게 정한 규칙입니다. 이름만 보고도 타입의 성격(값인지, 열거형인지, bool인지)을 알 수 있게 하는 것이 목적입니다. null일 수 있는지는 타입(`T?`)이 말합니다.

| 대상 | 규칙 | 예 | 코드 |
|---|---|---|---|
| 타입·열거 값·public 멤버 | PascalCase | `Ship`, `MaxSpeed` | L101 |
| struct | `S` 접두사 | `SVec2` | L102 |
| enum | `E` 접두사 | `ETeam` | L103 |
| interface | `I` 접두사 (.NET 프리셋도 같음) | `IDamageable` | L114 |
| 지역 변수·매개변수 | camelCase | `hitCount` | L104 |
| 메서드 | public은 PascalCase, 그 밖은 camelCase | `Fire()`, `reload()` | L105 |
| 상수 (`const`) | ALL_CAPS | `MAX_HP` | L106 |
| bool 변수 / private bool 멤버 | `b` / `mb` 접두사 | `bAlive`, `mbDirty` | L107 |
| bool을 돌려주는 메서드·속성 | `Is`/`Can`/`Has`/`Should` (자연스러우면 3인칭 동사도 허용: `Overlaps`) | `IsDead()` | L108 |
| private 필드 | `m` + PascalCase | `mTimer` | L109 |
| 약어 | 뒤에 단어가 이어지면 첫 글자만 대문자 | `HudScale`, `TargetID` | L112 |
| 한 글자 이름 | 단순한 루프 인덱스에만 | `i`, `j` | L113 |
| 중괄호 | 여는 중괄호는 새 줄(Allman) | | L204 |
| `var` | 타입이 뻔한 경우(`new`, `Spawn`, 캐스트, 컬렉션·배열 초기화)에만 | | L202 |
| 멤버 순서 | 필드 → 속성 → 생성자 → 메서드, 각각 public 먼저 | | L205 |

`dotnet` 프리셋은 .NET 기본 이름 규칙(private 필드 `_camelCase`, 상수 PascalCase, 접두사 규칙 없음)을 씁니다.

`game` 프리셋은 `standard`와 같고, 숫자·벡터(int·float·char·enum·struct) 지역 변수와 매개변수의 한 글자 이름(`x`, `y`, `r`, `t`, `n`, `i`, `v` …)을 허용합니다(L113). 수학과 좌표 코드에서 L113이 힌트의 대부분이 되기 때문입니다. 핸들·문자열·컬렉션의 한 글자 이름은 여전히 알립니다. 모든 프리셋에서 `n / W` 옆에 같은 피연산자의 `n % W`가 있으면 칸 번호 → 좌표 계산으로 보고 L305(정수 나눗셈의 소수점 손실)를 알리지 않습니다.

**린트가 아니라 문법인 것.** C# 코딩 표준이 사람에게 맡기는 몇 가지는 Sijo에서 컴파일 오류이거나 타입이 말하므로 린트 규칙이 없습니다. 오류는 프리셋이나 `.sijolint`로 끌 수 없습니다.

| C# 코딩 표준의 규칙 | Sijo |
|---|---|
| 항상 중괄호, 빈 본문 `if (x);` 금지 | 본문은 항상 블록 — E0031 (1.4.1절) |
| 한 줄에 선언 하나 | 선언 하나에 이름 하나 — E0030 |
| switch 문에 `default:` | switch 문은 모든 값을 다룸 — E0237 (1.7절) |
| 재귀 함수의 `Recursive` 접미사 | `[Recursive]` 특성, 호출 그래프로 검사 — E0312/W0312 (1.6.3절) |
| null일 수 있는 값의 `OrNull` 접미사 | `T?` 타입이 말함 |

이름 접미사가 할 일을 선언이 합니다. null일 수 있는 핸들은 `T?`, 재귀 함수는 `[Recursive]`, 컬렉션을 넘겨받거나 돌려주는 곳은 `own`이 코드에 보이고, 컴파일러가 모두 검사합니다.

**쓰지 않는 `mut` (L316).** 값은 기본이 불변이므로(1.6.2절) `mut`은 "이 값은 바뀐다"는 선언입니다. 한 번도 바뀌지 않는 지역 변수·매개변수·엔티티 필드에 `mut`이 붙어 있으면, 값이 고정되어 있다는 사실이 가려집니다. 엔티티 필드에서는 이 차이가 더 큽니다. `mut`이 없는 필드는 생성할 때 복제된 뒤로 변경을 보낼 일이 없고 해저드 이중 버퍼가 되지 않는다는 것이 선언만으로 보장되는데, 쓰지 않는 `mut`은 그 보장을 읽는 사람에게서 가립니다. L316은 두 프리셋 모두 경고이고, 고치는 방법은 `mut`을 지우는 것입니다. 본문이 없는 선언(인터페이스·추상 메서드)의 매개변수는 제외하고, `for (mut int i = 0; ...; i++)`처럼 머리만 바꾸는 변수에 붙은 `mut`도 알립니다(머리의 증감식은 `mut` 없이 바꿀 수 있음).

### 코딩 표준이 Sijo와 만나는 지점

- **이름 대신 타입과 특성.** 코딩 표준은 이름만 보고 값의 성격을 알 수 있게 하려는 것입니다. Sijo에서는 그중 컴파일러가 검사할 수 있는 것을 선언에 적습니다: null일 수 있는 핸들은 `Ship? Target`, 재귀 함수는 `[Recursive]`, 소유권이 넘어가는 컬렉션은 `own`, 바뀌는 값은 `mut`. 이름 규칙은 쓰는 곳에서 보이지 않는 나머지 성격(struct인지, 열거형인지, bool인지, private인지)을 알리는 데 씁니다.
- **`b`/`mb` 접두사** 와 **`m` 접두사** 는 타입 검사 결과(bool인지, private인지)를 씁니다.
- **`var` 대신 명시 타입** 의 자동 수정은 추론된 실제 타입으로 바꿉니다(`var dir` → `SVec2 dir`).

### 게임 규칙 — 컴파일러만 아는 것

| 코드 | 예 | 왜 중요한가 |
|---|---|---|
| L401 | `other.Target = this;` | 여러 엔티티가 같은 시스템에서 쓰면 마지막(엔티티 순서) 것만 남음. `+=`나 메시지 메서드 권장 |
| L402 | 엔티티 시스템 안의 `foreach (var o in Query<T>())` | 엔티티마다 전부 훑는 O(N·M) 루프. 공간 격자로 바뀌지 않은 이유와 고치는 방법을 함께 알려 줌: 반지름이 상대 엔티티의 값에 따라 달라지거나 시스템이 반지름 필드를 씀(→ `d < MAX_R * MAX_R && ...`처럼 상수 상한을 더함), 거리 검사 앞에 부수 효과가 있는 문장, 상수나 자기 반경이 아닌 값과 `Sqrt`로 비교(상수·자기 반경과 비교하면 가속됨), 어떤 시스템도 `this`에 대해 부르지 않는 메서드 안의 루프(→ 시스템이나 그 헬퍼로 옮김) 등 |
| L403 | 엔티티 `Update` 안의 `Print` | 출력 순서 때문에 그 시스템 전체가 직렬로 실행됨 |
| L404 | 범위 없이 복제되는 float | 32비트 그대로 전송. `[Precision(0.01)] public mut float in 0..64 X`이면 13비트 |
| L405 | 클라이언트 코드의 `Random` | 서버가 모르는 값 → 클라이언트와 서버가 어긋남 |
| L406 | 이웃을 읽으며 자신을 쓰는 필드 | 이중 버퍼링(틱마다 memcpy)이 들어간다는 안내 |
| L407 | 핸들을 쓰는 `Spawn` | 생성 순서대로 id를 정하려고 시스템이 직렬로 돎. 문장으로만 쓰면 병렬 |
| L408 | 루프 안 `new List` | 반복마다 할당. 루프 밖에서 만들고 `Clear()` |
| L409 | 엔티티 시스템(과 그 시스템이 부르는 메서드·정적 함수) 안의 `extern` 호출 | 호스트 호출은 순서대로 일어나므로 그 시스템 전체가 직렬로 돎(경고에 시스템 이름). void면 `[Deferred]`, 아니면 필드에 모았다가 `Game.Update`에서 한 번 |

### 자동 수정 (`--fix`)

- **이름 변경**: 의미 인덱스(모든 심볼의 정의·참조 위치)로 바꾸므로, 같은 이름의 다른 심볼이나 문자열·주석은 건드리지 않습니다. 새 이름이 이미 쓰이고 있거나 키워드이면 자동 수정하지 않습니다. 실행 결과에 이름이 드러나는 것도 알리기만 하고 고치지 않습니다("not fixed automatically"): 열거형 멤버(출력되는 이름), 엔티티 클래스 이름(실행 요약·저장 상태), `Random`을 쓰는 함수와 그 타입(난수는 호출 지점의 이름으로 정해짐), C 이름을 따로 적지 않은 `extern` 함수, 함께 바뀌어야 하는 메서드 묶음(인터페이스 메서드와 구현, 가상 메서드 재정의 사슬 — 편집기의 이름 변경을 씀). 접두사 제안은 대소문자를 바로잡습니다(`iShape` → `IShape`).
- **`var` → 명시 타입**, **`x == true` → `x`**, **여러 줄 블록의 `{` 줄바꿈**. 본문의 중괄호는 문법이므로(E0031) 린트 수정 대상이 아닙니다. C# 스타일로 쓴 코드는 `compiler/tools/autofix.py`가 컴파일 오류 중 고칠 방법이 하나뿐인 것(`mut`·`own`·`[Recursive]`·중괄호)을 고칩니다.
- 안전장치: 수정을 적용할 때마다 다시 컴파일해 보고, 컴파일되지 않는 결과는 절대 쓰지 않습니다(한 묶음 중 하나가 깨뜨리면 나머지는 적용). 이름 변경은 다른 수정과 다른 패스에서 적용합니다. 테스트(`fix_all_preserves_behaviour`, `fix_all_keeps_names_the_program_shows`)는 수정 전후 실행 결과가 같음을 확인합니다.

### 설정

`.sijolint` 파일을 소스 옆이나 상위 폴더에 둡니다.

```ini
preset = standard      # standard(기본, Sijo 코딩 표준) | game (standard + 수학·좌표용 한 글자 이름) | dotnet (.NET 기본 이름 규칙: _camelCase 등)
L113 = off             # 규칙 끄기
unused-local = error   # 이름으로도 지정 가능, 수준: off | hint | warning | error
max-complexity = 20
```

소스 안에서는 `// sijo-lint: disable L204`(파일 전체), `// sijo-lint: disable-next-line L403`, `// sijo-lint: disable-line L304`로 끌 수 있습니다.

**출력과 종료 코드.** 오류만 stderr로, 경고·힌트·요약은 stdout으로 나갑니다(stderr에 무엇이든 쓰면 실패로 보는 셸, 예를 들어 PowerShell에서 성공한 빌드가 실패로 보이지 않도록). `build`/`run`/`check --quiet`는 오류만 출력합니다(`run`은 프로그램에도 `--quiet`를 넘김).

**스케줄 기준선.** `build`/`run`은 엔티티 시스템마다 병렬인지를 `.sijo/schedule`(프로젝트 디렉터리, 파일 하나면 그 옆의 `.sijo/<이름>.schedule`)에 남깁니다. 기준선에서 병렬이던 시스템이 직렬이 되면(지나가며 넣은 extern 호출·`Print`·핸들을 쓰는 `Spawn`) `schedule: Mob.Update ran in parallel in the baseline and now runs serially: <이유>`를 원인을 고치거나 `--accept-schedule`로 새 스케줄을 받아들일 때까지 빌드마다 알립니다. 그 밖의 변화(새 시스템, 직렬 → 병렬)는 조용히 기록됩니다. `.sijo/`는 `.gitignore`에 넣거나, 팀이 함께 보려면 커밋합니다.

`lint`·`check`·`run`·`build`·`explain`·`live`·편집기가 `.sijolint`를 같은 방식으로 읽습니다. `error`로 올린 규칙이 걸리면 `lint`처럼 `check`·`build`·`run`도 종료 코드 1로 실패합니다(경고는 실패하지 않음). 모르는 규칙이나 잘못된 값은 오류이고 명령은 종료 코드 2로 멈춥니다(라이브 코딩은 알리고 기본 설정으로 계속 돕니다). 편집기에서는 문서 맨 위의 진단으로 보입니다.

## 10.2 인텔리센스 — `sijoc lsp`

표준 Language Server Protocol 서버입니다. VS Code(`editors/vscode`), Neovim, Helix, Emacs 등 LSP를 지원하는 편집기에서 씁니다.

| 기능 | 내용 |
|---|---|
| 진단 | 매 입력마다 컴파일 오류·경고 + 린트 결과 |
| 자동 완성 | `.` 뒤 멤버(구조체 필드·메서드, 엔티티 필드·메서드, 리스트, enum 멤버, 상수, `Math`/`Random`/`Time`/`Draw`), 범위 안의 지역 변수·타입·키워드. 범위 타입의 경계(`int in |`, `0..|`)에서는 숫자 상수만 |
| 호버 | 쓴 그대로의 선언(`mut int in 0..100 Hp`, `own List<int> xs`, `client void Render()`, struct의 `mut` 메서드)과 **컴파일러의 결정**: 소속 ECS 컴포넌트와 접근 시스템, 복제 여부와 비트 수(근거 포함), 범위 계약, 지연 쓰기 방식(ADD/SET), 해저드 이중 버퍼, 시스템의 스테이지와 병렬 여부, 함수 효과(인터페이스·가상 호출이면 구현들의 효과를 합쳐 보여 주고 몇 개 중 하나로 디스패치되는지 알려 줌, 기본 구현을 쓰는 아키타입은 기본 구현으로), 인터페이스를 구현한 아키타입, 리스트의 메모리 영역. 제네릭 엔티티 클래스(`Pool<T>`)는 인스턴스(`Pool<int>`, `Pool<SVec2>`)와 각 아키타입을 나열하고, 그 멤버는 첫 인스턴스로 보여 주며 어느 제네릭 클래스에서 왔는지 알려 줌 |
| 정의 이동 / 참조 찾기 / 이름 변경 | 의미 인덱스 기반 (주석·문자열·다른 동명 심볼 제외). 인터페이스 메서드·그 기본 구현과 구현(클래스·기반 클래스·struct, struct가 쓰는 기본 구현)·제네릭 인터페이스와 제네릭 엔티티 클래스의 모든 인스턴스·가상 메서드 재정의 사슬은 함께 바뀜. 새 이름이 키워드이거나 이미 쓰이거나 다른 심볼을 가리게 되면 이유와 함께 거절하고, 한글 같은 유니코드 이름은 받음 |
| 빠른 수정 | 린트 자동 수정 + "Fix all" |
| 인레이 힌트 | 필드 옆 `Ship_C0 · net 26 bits`, 시스템 옆 `stage 1 · parallel`, `new List` 옆 `→ frame arena · reserved`, 원격 쓰기 옆 `⇢ deferred ADD`, 클라이언트 호출 옆 `⇢ RPC to server`, `var` 옆 추론 타입 |
| 문서 개요 | 타입과 멤버 트리 |

입력 중이라 코드가 잠시 컴파일되지 않을 때는 마지막으로 성공한 분석 결과로 완성·호버를 계속 제공합니다. 위치 계산은 LSP 규격대로 UTF-16 단위를 써서 한글 주석·식별자가 있어도 정확합니다.

## 10.3 라이브 코딩 — `sijoc live`

```bash
sijoc live examples/arena.sijo            # 실시간(60Hz)으로 게임 실행
sijoc live examples/arena.sijo -- --render-every 60   # 1초마다 클라이언트 화면 출력
```

파일을 저장하면:

1. 새 코드를 컴파일합니다(개발 모드, 약 0.3–0.5초).
2. **실패하면** 오류를 보여주고, 실행 중인 게임은 이전 빌드로 계속 돕니다.
3. **성공하면** 실행 중인 게임에 인계를 요청합니다. 게임은 현재 틱 끝에서 월드 상태를 저장하고 종료하고, 새 빌드가 같은 틱부터 이어서 실행합니다.

### 상태 이주 (스키마 마이그레이션)

상태는 **아키타입 이름 + 필드 이름 + 타입 서명**으로 저장되므로 코드가 바뀌어도 대부분 그대로 넘어갑니다.

| 변경 | 처리 |
|---|---|
| 필드 추가 | 새 필드는 선언된 기본값으로 시작 (`new:Counter.Bonus`) |
| 필드 삭제 | 저장된 값은 버림 (`dropped:Counter.Old`) |
| 필드 타입 변경 (`int` → `float`, 구조체 멤버 변경) | 타입 서명이 다르면 기본값으로 시작 |
| 범위를 더하거나 좁힘 (`int` → `int in 0..100`) | 값을 새 범위로 포화 (`saturated:Ship.Hp`) |
| `mut`을 더하거나 뺌 | 상태와 무관 |
| 엔티티 클래스 삭제 | 그 엔티티들은 사라지고, 그들을 가리키던 핸들은 세대 검사로 안전하게 null 처리 |
| 컴포넌트(1.16절) 필드 추가·삭제 | 위와 같음. 컴포넌트 필드는 `Burning.Left`처럼 컴포넌트 이름과 함께 저장되고, 조합 테이블은 `Ship+Burning`으로 저장됨 |
| 컴포넌트 삭제, 또는 그 조합에 더는 도달할 수 없음 | 같은 클래스에서 저장된 컴포넌트를 가장 많이 가진 테이블로 옮기고, 없어진 컴포넌트의 값은 버림 (`components-dropped:Ship+Burning+Wet`) |
| 메서드·시스템·컴포넌트 배치 변경 | 상태와 무관 (레이아웃은 컴파일러가 다시 계획) |

엔티티 핸들(인덱스·세대)과 빈 슬롯 순서까지 보존되므로, **코드를 바꾸지 않고 저장·복원하면 시뮬레이션 결과가 비트 단위로 같습니다**(테스트 `state_hand_over_is_exact`). 라이브 코딩은 개발 모드로 컴파일하므로 "죽은 필드 제거"를 하지 않습니다. 지금은 아무도 읽지 않는 필드도 다음 편집에서 읽기 시작할 수 있기 때문입니다.

같은 기능을 직접 쓸 수도 있습니다: `--save-state 파일`, `--load-state 파일`. 샤드 프로세스의 체크포인트도 이 형식이며(샤드 상태는 옆의 `.meta`), 샤드가 다시 시작할 때 `--recover`가 읽습니다(10.10절, 5.7.7절). 슬롯 표와 행 순서까지 보존되므로 복구한 샤드의 재시뮬레이션이 원래 실행과 비트 단위로 같습니다.

### 왜 프로세스 재시작 방식인가

같은 프로세스 안에서 코드를 바꿔 끼우는 방식(dlopen 핫 패치)은 컴포넌트 레이아웃이 바뀌는 순간 메모리 안의 데이터가 새 코드와 맞지 않게 됩니다. Sijo는 레이아웃을 컴파일러가 결정하므로, **레이아웃과 무관한 이름 기반 상태 형식**을 거쳐 새 프로세스로 넘기는 편이 안전하고 단순합니다. 비용은 재컴파일 시간뿐입니다.

## 10.4 한글 식별자

C#처럼 유니코드 식별자를 지원합니다. `class 배 : Entity { int 체력; }`처럼 쓸 수 있습니다. C 코드에서는 `_uXXXX_` 형태로 바뀌고, 출력과 보고서에는 원래 이름이 나옵니다. 대소문자가 없는 문자로 시작하는 이름에는 대소문자 관련 린트 규칙을 적용하지 않고, 접두사 뒤에 대소문자가 없는 글자가 오면 접두사 규칙을 만족한 것으로 봅니다(`S벡터`, `m속도`, `b살아있음`).

## 10.5 여러 파일 프로젝트

```
mygame/
  sijo.toml          (name = "mygame")
  Game.sijo
  units/Ship.sijo
  units/Rock.sijo
```

- `sijoc run mygame`, 또는 프로젝트 안의 아무 파일(`sijoc run mygame/units/Ship.sijo`)을 주면 프로젝트 전체를 컴파일합니다. `sijo.toml`이 없으면 디렉터리 아래의 모든 `.sijo` 파일, 파일 목록을 주면 그 파일들입니다.
- 내부적으로 파일들을 정해진 순서로 이어 붙여 한 번에 분석하고, **소스 맵**이 위치를 원래 파일·줄로 되돌립니다. 그래서 모든 진단, `explain`, `lint --fix`(수정이 파일별로 다시 써짐), 라이브 코딩(어느 파일을 저장해도 재컴파일)이 프로젝트 단위로 동작합니다.
- 언어 서버는 열린 문서가 `sijo.toml` 프로젝트에 속하면 프로젝트 전체(편집기에 열린 다른 파일은 저장 전 내용)를 분석하고, 파일별로 진단을 보내며, 다른 파일로의 정의 이동·참조 찾기·이름 변경을 지원합니다(테스트 `language_server_understands_projects`).

## 10.6 디버깅: `#line`과 패닉 위치

생성된 C에는 문장마다 `#line 42 "units/Ship.sijo"`가 들어갑니다.

- 런타임 오류는 .sijo 위치를 알려 줍니다:
  ```
  sijo panic: list index out of range
    at units/Ship.sijo:42
  ```
  (범위 밖 인덱스, 0으로 나누기, 없는 사전 키, 중복 `Add`, `Substring` 범위, `Parse` 실패, 완전하지 않은 switch 식에 맞지 않는 값)
- `sijoc build game --debug`는 `-O0 -g`로 빌드합니다. gdb에서 `break units/Ship.sijo:42`, 한 줄씩 실행, 새니타이저 스택 추적이 모두 .sijo 줄 기준입니다.

## 10.7 엔진에 넣기: `--lib`, `--host`, `--externs`, `extern`

```bash
sijoc build examples/dodge --lib -o build/dodge.c       # build/dodge.c + build/sijo_game.h (main 없음)
cc -std=gnu11 -O2 -ffp-contract=off -fopenmp -c build/dodge.c -o build/dodge.o
cc build/dodge.o my_engine.c -Ibuild -fopenmp -lm -o game          # 엔진이 main을 가짐
cc -std=gnu11 -O2 -ffp-contract=off -fopenmp -fPIC -shared build/dodge.c host_externs.c -lm -o libdodge.so   # 공유 라이브러리
# 한 번에: 호스트 C 파일을 함께 컴파일
sijoc build examples/dodge --host examples/dodge/host_term.c --ldflags -lncurses --exe dodge
./dodge                     # 터미널에서 플레이 (화살표/A D 이동, R 재시작, Q 종료)
./dodge --headless 900      # 터미널 없이 정해진 입력으로 900틱 (테스트용)
```

- `--lib`: `main` 대신 임베딩 API를 넣은 C 파일과 헤더를 만듭니다. `--lib-prefix mygame`이면 함수 이름이 `mygame_create`…, 헤더가 `mygame.h`가 되어 게임 여러 개를 한 프로그램에 링크할 수 있습니다.
- `--host host.c`: 호스트 프로그램(자기 `main`과 `extern` 함수 정의)을 게임과 함께 컴파일합니다.
- `--externs ext.c`: `extern` 함수만 C로 정의하고 sijoc의 `main`을 그대로 씁니다. 그래서 `--server`/`--connect` 네트워크 실행이 그대로 됩니다. 정의되지 않은 `extern` 함수가 있으면 빌드가 무엇을 넘겨야 하는지 알려 줍니다.
- 컴파일 옵션: gcc·clang(MinGW 포함) `-std=gnu11`. 병렬 시스템은 `-fopenmp`, 링크는 `-lm`(Windows는 `-lws2_32` 추가). Windows DLL은 게임 쪽에 `SIJO_GAME_BUILD_DLL`, 호스트 쪽에 `SIJO_GAME_USE_DLL`을 정의합니다. 헤더는 `extern "C"`라 C++에서 바로 씁니다.
- 결과는 어떤 최적화 옵션에서도 비트 단위로 같습니다. 정수 연산은 런타임이 부호 없는 연산으로 wrap을 직접 구현하므로 `-fwrapv`가 필요 없고, 결과를 바꿀 수 있는 `-ffast-math`와 x87 부동소수점은 컴파일 단계에서 `#error`로 거부합니다. 생성 코드는 FMA 축약을 끄는 pragma(clang `#pragma STDC FP_CONTRACT OFF`, gcc `#pragma GCC optimize("fp-contract=off")`)를 넣고, 직접 컴파일할 때는 `-ffp-contract=off`도 함께 주기를 권장합니다(`sijoc build`는 자동으로 붙임).

헤더(`sijo_game.h`) API:

| 함수 | 내용 |
|---|---|
| `sijo_game_create(seed)` | 월드를 만들고 `Game.Start` 실행. 메모리 부족이면 `NULL`, `Game.Start`의 패닉은 `sijo_game_error`로 확인 |
| `sijo_game_step(g)` | 한 틱(1/60초): 서버 시스템 → 델타 복제 → 클라이언트 시스템(입력 → RPC). 1 성공, 0 패닉으로 멈춤, -1 잘못된 호출 |
| `sijo_game_error(g)` | 패닉 메시지(.sijo 파일:줄 포함) 또는 `NULL` |
| `sijo_game_draw_count(g)` / `sijo_game_draw(g, i)` | 마지막 클라이언트 프레임의 그리기 목록(스프라이트 또는 UTF-8 텍스트). 범위 밖 `i`는 `text_len == -1` |
| `sijo_game_tick`, `sijo_game_checksum` | 진행 틱, 레이아웃과 무관한 월드 해시(결정론 검증용) |
| `sijo_game_save(g, path)` / `sijo_game_load(g, path)` | 상태 저장·복원(라이브 코딩과 같은 형식). 복원은 트랜잭션이라 실패하면 월드가 그대로이고, 성공하면 패닉으로 멈춘 게임도 다시 돕니다 |
| `sijo_game_set_threads(g, n)` | 병렬 시스템의 스레드 수(1~64). 결과는 스레드 수와 무관 |
| `sijo_game_set_panic_hook(hook, user)` | 패닉 메시지를 받을 콜백(모든 게임 공통) |
| `sijo_game_main(argc, argv)` | 독립 실행 파일과 같은 명령줄(`--server`, `--connect`, `--ticks` …) |
| `sijo_game_destroy(g)` | 게임이 가진 모든 메모리 해제(`NULL`은 무시) |

`extern` 함수의 C 프로토타입은 헤더 끝에 생성됩니다.

**엔티티 필드 읽기.** 엔티티 클래스의 public 필드마다 서버 월드를 읽는 함수가 헤더에 생깁니다. `i`는 그 클래스(하위 클래스 포함)의 살아 있는 엔티티를 테이블 순서로 0..count-1까지 셉니다. 값과 포인터는 다음 step·load·destroy까지 유효합니다.

```c
int32_t n = sijo_game_World_count(g);                 // World 엔티티 수
uint64_t id = sijo_game_World_id(g, 0);               // (세대 << 32) | 슬롯
int32_t len; const int32_t* cells = sijo_game_World_G(g, 0, &len);   // int[] G: 포인터와 길이
float x = sijo_game_Ship_Pos_X(g, i);                 // struct 필드는 멤버마다
const char* name = sijo_game_World_Name(g, 0, &len);  // 문자열: UTF-8, NUL로 끝나지 않음
```

숫자·bool·char·enum·문자열·핸들(`uint64_t`)·그것들로 된 struct·숫자의 배열과 리스트를 읽을 수 있고, 범위 밖 `i`는 0(포인터면 `NULL`, `*len = -1`)입니다. `--lib` 빌드에서는 Sijo 코드가 읽지 않는 public 필드도 호스트가 읽을 수 있도록 저장소를 남깁니다(죽은 필드 제거에서 빠짐). 이름이 API 함수와 겹치면 뒤에 번호가 붙습니다.

**호스트 함수에 배열 넘기기.** `extern` 매개변수가 숫자 배열·리스트이면 C에서는 `const int32_t* cells, int32_t cells_len`(float는 `const float*`, bool은 `const bool*`)로 받습니다. 호출하는 동안만 유효한 읽기 전용 포인터라 복사가 없습니다. `[Deferred]` extern이면 인자가 복사되어 쌓였다가 스테이지 끝에 같은 형태로 전달됩니다(1.11절).

**오류와 스레드.** 게임 코드의 런타임 패닉(범위 밖 인덱스, 없는 사전 키, 0으로 나누기, 메모리 부족 …)은 프로세스를 끝내지 않습니다. 호출이 0을 돌려주고 `sijo_game_error`가 메시지를 주며, 그 게임은 저장된 상태를 복원할 때까지 더 진행하지 않습니다. 병렬 시스템의 작업 스레드에서 난 패닉도 같은 방식으로 잡습니다. 게임의 호스트 콜백 안에서 그 게임을 다시 부르면 -1입니다. 게임 하나는 한 번에 한 스레드만 쓸 수 있고, 서로 다른 게임은 독립적이라 각자 다른 스레드에서 돌려도 됩니다. 엔진의 다른 스레드나 다른 프로세스와 CPU를 나눠 쓴다면 `OMP_WAIT_POLICY=passive`를 권장합니다. OpenMP 기본 정책은 작업 스레드가 다음 일을 바쁘게 기다리므로, CPU가 모자라면 틱이 10배 넘게 느려질 수 있습니다(11.4절).

**C 이외의 언어.** 공유 라이브러리로 빌드하면 C#(P/Invoke), Python(ctypes), Rust·Go(FFI) 등에서 부를 수 있습니다. `extern` 함수는 C 심볼이어야 하므로, 이런 호스트는 함수 포인터를 받아 넘겨주는 몇 줄짜리 C 파일을 함께 링크합니다.

```csharp
[DllImport("dodge")] static extern IntPtr sijo_game_create(ulong seed);
[DllImport("dodge")] static extern int sijo_game_step(IntPtr g);
[DllImport("dodge")] static extern ulong sijo_game_checksum(IntPtr g);
```

```python
lib = ctypes.CDLL("./libdodge.so")
lib.sijo_game_create.restype = ctypes.c_void_p
lib.sijo_game_step.argtypes = [ctypes.c_void_p]
g = lib.sijo_game_create(42)
```

검증: C·C++·C#·Python·Rust·Go 호스트가 같은 게임에서 같은 그리기 목록과 체크섬을 내고, 한 언어에서 저장한 상태를 다른 언어 호스트가 복원해도 결과가 같습니다(11장). 테스트 `tests/embed.rs`가 API 전체(두 게임 동시 실행, C++ 헤더, 패닉 복구, 트랜잭션 복원, 스레드 수 무관성, 메모리 해제)를 확인합니다.

게임 쪽에서는 `extern` 함수로 엔진 기능을 부릅니다(1.11절). 예제 Dodge에서 키보드는 `client` 메서드 `Input()`이 `Host.IsKeyDown(EKey.Left)`를 읽고, 그 결과로 부르는 `Steer(-1)`은 컴파일러가 자동 RPC로 바꿔 서버 권한으로 실행합니다. 즉 같은 게임 코드를 `--server`/`--connect`로 네트워크 게임으로 돌릴 수도 있습니다. `Host.Bell()`은 서버 코드에서 부르는 외부 효과입니다. 테스트 `dodge_runs_embedded_in_a_c_host`가 이 경로 전체를 ncurses 없이 컴파일·실행하고 재현성을 확인합니다.

## 10.8 검사 모드: `--checked`

기본 정수 연산은 C#의 `unchecked`처럼 wrap합니다. `sijoc build --checked`(또는 `run`)로 빌드하면 C#의 `checked`처럼 정수 오버플로, `int.MinValue / -1`, 범위를 벗어난 float → int 변환이 .sijo 위치와 함께 패닉합니다. 컴파일 타임 계산(상수 접기·CTFE)은 오버플로하는 식을 미리 계산하지 않고 실행 시점 검사로 남겨서, 상수식도 같은 위치에서 패닉합니다. 개발 중 버그를 찾는 용도이며 결정론은 그대로입니다(같은 입력이면 같은 지점에서 패닉). 패닉하지 않는 프로그램은 두 모드의 결과가 같습니다.

## 10.9 결정론과 지원 플랫폼

같은 프로그램, 같은 시드, 같은 입력이면 아래 어느 조합에서도 월드 체크섬과 출력이 **비트 단위로 같습니다**. 매 커밋 테스트(`tests/cross.rs`)와 수동 행렬 검증(11장)으로 확인합니다.

| 축 | 검증한 값 |
|---|---|
| 컴파일러 | gcc 13, clang 18, MinGW-w64 gcc |
| 최적화 | `-O0`, `-O2`, `-O3 -march=native`, `-mfma` |
| CPU·OS | x86-64 Linux, AArch64(qemu), s390x 빅엔디언(qemu), x86-64 Windows(wine) |
| 스레드 | 1, 4 (OpenMP 없이 빌드해도 같음) |
| 네트워크 | 서로 다른 플랫폼의 클라이언트가 같은 서버에 붙어도 같은 복제 상태 해시 |

이를 위해 런타임은 다음을 직접 구현합니다: wrap 정수 연산, 수학 함수(1.13절), 숫자 ↔ 문자열 변환, 엔디언과 무관한 해시·직렬화, NaN 정규화. 플랫폼마다 다를 수 있는 것은 성능뿐입니다.

## 10.10 분산 실행 옵션 (샤드)

생성된 실행 파일 하나가 샤드 프로세스도 됩니다. 모델과 프로토콜은 5.7절, 컴파일러의 결정은 `sijoc explain`의 `[shards]`에 있습니다.

| 옵션 | 누구에게 | 내용 |
|---|---|---|
| `--shards N` | 한 프로세스 | 샤드 월드 N개(최대 64)를 한 프로세스에서 같은 교환으로 실행. 결과는 `--shards 1`과 같고, 요약에 `shards:`·`shard exchange:` 줄이 붙음 |
| `--shard-of N` | 모든 샤드 | 샤드 프로세스 수(2~64). 모든 샤드에 같은 값 |
| `--shard-id I` | 모든 샤드 | 이 프로세스의 샤드 번호(0..N-1). 0이 코디네이터 |
| `--peers A0,A1,...` | 모든 샤드 | 샤드 번호 순의 `HOST:PORT` N개(IPv6는 `[::1]:7001`). 자기 항목의 포트에 UDP로 바인드 |
| `--ticks N` | 코디네이터 | 실행할 틱 수. 다른 샤드는 코디네이터의 멈춤 표시를 따르므로 줄 필요 없음(0 = 멈출 때까지) |
| `--server PORT` | 코디네이터 | UDP 클라이언트의 게이트웨이(5.5.1의 서버 옵션 `--wait-clients`·`--budget`·`--tick-rate` 등이 그대로 적용). 없으면 프로세스 안 클라이언트(기본 실행 방식) |
| `--key HEX`, `--sim-loss P` 등 | 모든 샤드 | 샤드 링크에도 적용(암호화, 손실·지연·지터·중복 시뮬레이션). 코디네이터에서는 클라이언트 링크에도 적용 |
| `--checkpoint-dir DIR` | 모든 샤드 | 체크포인트와 메시지 로그를 켬. `DIR/shardI-TICK.state`(`--save-state` 형식) + `.meta` |
| `--checkpoint-every K` | 모든 샤드 | K틱마다 체크포인트(기본 60, 모든 샤드에 같은 값) |
| `--recover` | 다시 시작한 샤드 | 가장 새 체크포인트에서 이어서, 다른 샤드의 로그를 받아 다시 시뮬레이션(샤드 1..N-1) |
| `--shard-timeout-ms MS` | 모든 샤드 | 배리어에서 이만큼 기다리면 어느 샤드를 기다리는지 경고(기본 30000). 기다림은 계속됨 |
| `--die-at-tick T` | 시험용 | 틱 T가 끝나면 그 샤드가 바로 종료(복구 시험) |
| `--store DIR` | 한 프로세스·코디네이터 | `[Persistent]` 필드의 저장소(5.7.11). `DIR/<클래스>.store`를 생성 때 읽고, 코디네이터가 씀(샤드 프로세스에는 모두 같은 값) |
| `--store-every N` | 〃 | N틱마다 저장소 파일을 씀(기본 60). 실행이 끝날 때도 씀 |

- 실행이 끝나면 코디네이터가 다른 샤드의 행을 모아 월드 체크섬·복제 상태 해시·`--save-state`를 한 프로세스와 같게 출력하고, 샤드 프로세스들은 `== sijo shard I summary ==`(보낸 바이트·메시지, 이양, 재전송, 복구 때 다시 받은 메시지)를 출력합니다.
- 같은 기계에서는 샤드 프로세스가 코어를 나눠 쓰므로 `OMP_WAIT_POLICY`를 정하지 않으면 `passive`로 자신을 다시 시작합니다. `OMP_NUM_THREADS`는 코어 수 ÷ 샤드 수가 알맞습니다(`bench/shards.py`).
- `--shards`와 `--shard-of`는 함께 쓸 수 없고, `--profile`·라이브 코딩과도 함께 쓸 수 없습니다.

## 10.11 웹: `--target wasm32`

```bash
sijoc build examples/arena.sijo --target wasm32 -o web/arena.c   # web/: arena.c, sijo_game.h, arena.wasm, arena.js, index.html
python3 -m http.server -d web                                     # 브라우저로 http://localhost:8000
```

clang(wasm32 대상과 wasm-ld, 예: Ubuntu의 `clang`·`lld`)만 있으면 됩니다. emscripten·WASI SDK는 필요 없습니다. 생성되는 C에는 `runtime/sijo_wasm.h`가 C 라이브러리로 들어갑니다: `memory.grow` 위의 크기별 할당기, 문자열·메모리 함수, printf 계열(콘솔로 줄 단위), 로더가 메모리에 가진 파일(저장·복원), 정확한 `fmodf`, 소켓·시그널은 실패하는 대체 함수. 게임 계산은 네이티브와 **비트 단위로 같습니다**(테스트가 arena·boids·village와 격자 프로그램의 체크섬을 비교). OpenMP가 없으므로 한 스레드입니다.

- **로더** `<이름>.js`(ES 모듈): `const game = (await load(host)).create(42n);` 다음에 `step()`(1 성공, 0 패닉으로 멈춤), `error()`, `tick()`, `checksum()`(BigInt), `draws()`(`{ sprite, x, y, text }` 목록), `save(name)`/`load(name 또는 바이트)`, `fields.World_G(0)`(필드 읽기 함수: 숫자 배열은 typed array 뷰, 다음 step까지 유효), `destroy()`. node에서도 같은 파일을 씁니다.
- **호스트 함수**는 `host` 객체에 C 이름으로 줍니다: 문자열은 JS 문자열, 배열은 typed array(`Int32Array`/`Float32Array`/`Uint8Array`, 호출 중에만 유효), bool은 boolean으로 받고 돌려준 값은 C 타입으로 바뀝니다. 빠진 함수는 경고 한 번과 함께 아무것도 하지 않습니다. `host.print(fd, line)`이 출력을, `host.files`(`Map<string, Uint8Array>`)가 저장 파일을 받습니다.
- **패닉**: 게임 코드의 패닉은 JS 예외로 API 호출까지 풀리고, 로더가 스택 포인터를 되돌린 뒤 네이티브의 catch 지점과 같은 정리를 합니다. 그래서 `step()`이 0을 돌려주고 `error()`가 `.sijo` 파일:줄을 담은 메시지를 주며, 모듈은 멈추지 않고 저장된 상태를 `load`하면 다시 돕니다(longjmp 없이 trap으로 끝나던 문제).
- **페이지** `index.html`은 처음 한 번만 씁니다(고쳐 써도 다시 덮지 않음): 60틱/초로 돌리고 그리기 목록을 캔버스에 그리며(스프라이트 번호마다 색 사각형, 텍스트), 키 상태 집합과 호스트 함수 자리가 있습니다.
- 디버그 빌드는 `--debug`(`-O0 -g`), 추가 옵션은 `--cflags`/`--ldflags`, 다른 clang은 `--cc`.
