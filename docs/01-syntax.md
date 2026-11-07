# 1. 문법 — C# 기반

Sijo의 문법은 C# 12의 부분집합 + 게임용 확장 몇 가지입니다. C# 코드처럼 보이지만 의미론(값 의미론, 엔티티 핸들, 시스템 규칙)이 다릅니다. 문법에서 C#과 가장 크게 다른 점은 둘입니다. 값은 기본이 불변이고 바뀌는 것에 `mut`을 씁니다(1.6.2절). 값의 성질은 타입에, 코드의 성질은 수식어에 쓰고, 특성은 컴파일러에 주는 힌트로만 씁니다(1.6절).

## 1.1 전체 예제

```csharp
using Sijo;

namespace Asteroids;                       // 파일 범위 네임스페이스 (C# 10)

public struct SVec2                        // struct = 순수 값 타입 (코딩 표준: S 접두사)
{
    public float X;                        // struct 멤버에는 mut이 없음: 담고 있는 변수·필드를 따름
    public float Y;

    public float LengthSq => X * X + Y * Y;

    public SVec2(float x, float y)
    {
        X = x;
        Y = y;
    }

    public static SVec2 operator +(SVec2 a, SVec2 b) => new SVec2(a.X + b.X, a.Y + b.Y);
    public static SVec2 operator -(SVec2 a, SVec2 b) => new SVec2(a.X - b.X, a.Y - b.Y);
    public static SVec2 operator *(SVec2 a, float s) => new SVec2(a.X * s, a.Y * s);
}

public class Ship : Entity                 // : Entity → 컴파일러가 ECS로 분해
{
    [Spatial] public mut SVec2 Pos;        // 분산(샤딩) 기준 좌표. mut = 생성 뒤에도 바뀜
    public SVec2 Vel;                      // mut이 없으면 Spawn에서 정한 값 그대로
    public mut int in 0..100 Hp = 100;     // 범위 타입: 항상 0..100 → 7비트 양자화
    public mut float Cooldown;
    public mut int Kills;

    public void Update(float dt)           // Update → 시스템
    {
        Pos += Vel * dt;
        Cooldown -= dt;
        if (Cooldown <= 0)
        {
            Spawn(new Bullet { Pos = Pos, Vel = Vel * 3, Owner = this });
            Cooldown = 0.5;                // 실수 리터럴은 문맥에 맞춰 float
        }
        if (Hp <= 0)
        {
            Destroy();
        }
    }

    public client void Render()            // 클라이언트에서만 실행 → 여기서 읽는 필드가 자동 복제
    {
        Draw.Sprite(1, Pos.X, Pos.Y);
    }
}

public class Bullet : Entity
{
    public mut SVec2 Pos;
    public SVec2 Vel;
    public Ship? Owner;                    // 엔티티 참조 = 세대 핸들, 필드는 nullable(T?) 필수
    public mut float Life = 2;

    public void Update(float dt)
    {
        Pos += Vel * dt;
        Life -= dt;
        if (Life <= 0)
        {
            Destroy();
            return;
        }
        foreach (var ship in Query<Ship>())   // 다른 엔티티 읽기 = 시스템 시작 시점 스냅샷
        {
            if (ship != Owner && (ship.Pos - Pos).LengthSq < 1)
            {
                ship.Hp -= 10;             // 다른 엔티티 쓰기 = 지연 명령 (정해진 순서로 적용, 명령마다 범위로 포화)
                if (Owner is Ship owner)
                {
                    owner.Kills += 1;
                }
                Destroy();
                return;
            }
        }
    }
}

public static class Game
{
    public static void Start()             // 월드 초기화 (서버)
    {
        for (int i = 0; i < 64; i++)       // for 헤더의 변수는 증감식에서만 바뀜 (mut 불필요)
        {
            Spawn(new Ship { Pos = new SVec2(i * 4, Random.Float(-50, 50)), Vel = new SVec2(1, 0) });
        }
    }

    public static void Update(float dt)    // 전역 시스템 (모든 엔티티 시스템 뒤에 실행)
    {
        if (Time.Tick % 60 == 0)
        {
            Print($"tick {Time.Tick}: ships={Count<Ship>()} bullets={Count<Bullet>()}");
        }
    }
}
```

> 위 예제는 기본 린트 프리셋(Sijo 코딩 표준)을 따릅니다: 구조체 `S`·열거형 `E` 접두사, 상수 `ALL_CAPS`, 여는 중괄호는 새 줄(10장). 본문을 감싸는 중괄호, 선언 하나에 이름 하나, 바뀌는 값의 `mut`은 코딩 표준이 아니라 문법입니다(1.4.1절, 1.6절).

## 1.2 선언

| 선언 | 의미 | C#과의 차이 |
|---|---|---|
| `struct S { ... }` | 값 타입. 필드, 생성자, 메서드, 연산자 오버로딩 | 리스트·사전 필드 금지 → 항상 memcpy로 복사 가능한 POD. 멤버에는 `mut`이 없고, `this`를 바꾸는 메서드에 `mut`을 씀(1.6.2절) |
| `class C : Entity { ... }` | **엔티티 타입**. 객체가 아니라 ECS 아키타입으로 컴파일 | `new C()` 불가, `Spawn(new C { ... })`로만 생성. 변수는 핸들. 필드는 `mut`이어야 생성 뒤에 바뀜 |
| `class D : C { ... }` | 엔티티 상속 → D의 아키타입 = C의 컴포넌트 ∪ D의 컴포넌트 | 가상 호출은 아키타입별 단형화로 **정적 디스패치** |
| `static class G { ... }` | 정적 함수·상수 모음. `Game.Start/Update`는 진입점 | 변경 가능한 정적 필드 금지 (전역 가변 상태 = 데이터 레이스 원천) |
| `enum E { A, B }` | 정수 기반 열거형 | 동일 |
| `const T X = e;` | 컴파일 타임 상수 (CTFE로 계산). 타입은 무엇이든 됨: `const float HIT_RADIUS_SQ = HIT_RADIUS * HIT_RADIUS;`, `const SVec2 ORIGIN = new SVec2(0, 0);` | C#의 `const`는 기본 타입·문자열만 되고 나머지는 `static readonly`로 씀. Sijo에는 `readonly`가 없음(E0028): 상수는 모두 `const`. 메서드 안에서도 `const int N = Rules.MAX * 2;`처럼 지역 상수를 둘 수 있고, 값은 컴파일 타임에 정해져야 함(E0215) |
| `struct Pair<T> { ... }` / `class Pool<T> : Entity { ... }` / `static class Pairs<T>` / `T Max<T>(T a, T b)` | 제네릭 struct·엔티티 클래스·정적 클래스·메서드 (1.14절) | 타입 인자마다 따로 검사·컴파일(C++ 템플릿식). 엔티티 클래스는 인스턴스(`Pool<int>`)마다 아키타입이 따로 있음. 정적 클래스는 인스턴스(`Pairs<int>`)마다 함수와 상수가 따로 있음(정적 필드는 원래 없음). `where` 제약은 인스턴스마다 타입 인자에 대해 검사(E0260) |
| `interface I { void M(int x); bool Ok() => ...; }` / `interface I<T>` | 메서드 시그니처와 기본 구현 묶음 (1.15절). 엔티티 클래스(`: Entity, I`)와 struct가 구현, 제네릭 인터페이스는 타입 인자마다 인스턴스. 정적 멤버: `static abstract T Zero();`(구현하는 타입이 제공, `T.Zero()`)와 인터페이스 자신의 정적 함수 | 필드·`static virtual`·`{ get; }` 속성 없음. 기본 구현은 인터페이스의 멤버만 봄. 인터페이스 핸들은 구현한 아키타입별로 정적 디스패치 |
| `component C { ... }` | 엔티티가 런타임에 얻고 잃는 데이터 + 메서드 (1.16절). `e.Add(new C { ... })`, `e.Remove<C>()` | C#에 없음(Unity의 `AddComponent`, Verse Scene Graph의 컴포넌트에 해당). 상속·생성자·`async` 없음 |
| `delegate int Op(int a, int b);` | 델리게이트 타입 (1.14.1절). `Func<...>`/`Action<...>`도 됨 | null이 없음(기본값은 아무것도 하지 않음), 캡처는 값 복사, 호출은 정적 분기 |

## 1.3 타입

- 기본: `int`(i32), `float`(f32), `bool`, `char`(유니코드 코드 포인트, 1.8절), `void`
- 범위 타입: `int in 0..100`, `float in -1..1` — 값이 항상 그 범위 안에 있는 int·float (1.6.1절)
- 사용자: `struct`, `enum`, 엔티티 `class`, 제네릭 struct(`Pair<int>`)와 제네릭 엔티티 클래스(`Pool<int>`, 핸들은 `Pool<int>?`, 1.14절), 인터페이스 핸들(`IDamageable?`, `IContainer<int>?`, 1.15절)
- 엔티티 핸들: `Ship`(살아 있음이 보장된 핸들), `Ship?`(nullable 핸들)
- 델리게이트: `Func<int, int>`, `Action<string>`, `Action`, `delegate` 선언 (1.14.1절)
- 컬렉션: `List<T>`, 배열 `T[]`, 다차원 배열 `T[,]`·`T[,,]`, `Dictionary<K, V>` (K는 int/char/enum/string/핸들) — 1.9절, 1.14절. 원소는 기본/struct/enum/핸들/문자열과 **다른 리스트·배열**(`List<List<int>>`, `int[][]`, `List<int>[]`, `int[][,]`)입니다. 중첩 컬렉션은 바깥 컬렉션이 안쪽을 소유하며 함께 해제됩니다.
  - `List<T>`: 인덱서, `Count`, `Add`, `Insert(i, x)`, `Remove(x)`, `RemoveAt(i)`, `Clear`, `Contains`, `IndexOf`, `Reverse`, `Sort()`, `Clone`, 그리고 람다를 받는 메서드(1.14절). `Sort()`는 int·float·bool·char·enum·string 리스트를 C#의 `CompareTo` 순서(float은 NaN이 가장 작음, 문자열은 서수 비교)로 **안정 정렬**합니다. `Contains`·`IndexOf`·`Remove`의 float 비교는 C#처럼 NaN도 같은 값으로 봅니다.
  - 컬렉션 초기화: `new List<int> { 1, 2, 3 }`(어디서나), `new Dictionary<string, int> { { "a", 1 }, ["b"] = 2 }`(변수 선언에서), 배열 `new int[] { 1, 2 }` / `int[] xs = { 1, 2 };`, 다차원 배열 `new int[,] { { 1, 2 }, { 3, 4 } }` / `int[,] g = { { 1, 2 }, { 3, 4 } };`
- 문자열: `string`은 불변 값 타입입니다(지역 변수, 필드, struct 멤버, 리스트 원소, 사전 키·값). 1.8절. `Print`에 직접 전달되는 보간 문자열은 여전히 **메모리에 만들어지지 않고** 바로 출력 코드로 낮춰집니다(무할당 포맷팅).

## 1.4 문장과 식

C#과 같은 것: 블록, `var`/명시 타입 지역 변수(바뀌는 변수는 `mut`, 1.6.2절), `if/else`, `while`, `do/while`, `for`, `foreach (var x in ...)`, `return`, `break`, `continue`, 복합 대입(`+=` 등), `++/--`, 삼항 `?:`, 캐스트 `(float)x`, `is` 패턴, `==/!=` (핸들·문자열 비교), 보간 문자열(`{x:F2}` 형식 지정, 구멍 안의 삼항식은 C#처럼 괄호 필요, 구멍 안 문자열 리터럴 허용), `switch` 문과 `switch` 식(1.7절), `async`/`await`(1.10절), 람다(1.14절), `default(T)`·`default`(0, `false`, `""`, null 핸들, 필드가 모두 기본값인 struct), 컬렉션 초기화.

게임용 확장:

| 구문 | 의미 |
|---|---|
| `Spawn(new T { F = e, ... })` | 엔티티 생성 (시스템 종료 시 반영). 반환 타입 `T?` |
| `Destroy()` / `Destroy(h)` | 자기/다른 엔티티 파괴 (지연) |
| `foreach (var e in Query<T>())` | T와 그 하위 타입 엔티티 순회. 루프 변수는 아키타입별로 단형화된 커서 |
| `e.Add(new C { F = x })` / `e.Remove<C>()` / `e.Has<C>()` / `e.Get<C>() is C c` | 컴포넌트 붙이기·떼기(지연 구조 변경)·검사·조회 (1.16절). 받는 쪽이 없으면 `this` |
| `foreach (var (s, b) in Query<Ship, Burning>())` | Burning이 붙은 Ship 순회. 타입마다 커서 하나(같은 엔티티) |
| `Count<T>()` | 살아 있는 엔티티 수 |
| `atomic { ... }` / `fail;` | 트랜잭션 블록. `fail`하면 블록 안의 모든 쓰기(값, 리스트·배열·사전과 그 안의 컬렉션)·명령·`Spawn` 롤백 (Verse의 failure context). 중첩 가능(안쪽 `fail`은 안쪽만), `return`·`break`·`continue`로 나가면 커밋. 안에서는 `await`(E0803)와 `Print`·`Draw`(E0701) 불가 (6.2절) |
| `Time.Tick`, `Time.Dt` | 결정론적 시간 |
| `Random.Float(a, b)`, `Random.Int(a, b)` | 결정론적 카운터 기반 난수 |
| `Math.*` | `Sqrt, Abs, Min, Max, Clamp, Sign, Floor, Ceiling, Round`(짝수 반올림)`, Truncate, Sin, Cos, Tan, Asin, Acos, Atan, Atan2, Exp, Log, Log10, Log2, Pow`, 상수 `Math.PI, E, Tau`. 모든 플랫폼에서 비트 단위로 같은 결과(1.13절) |
| `int.MaxValue` 등 | `int.MinValue/MaxValue`, `float.MinValue/MaxValue/Epsilon/NaN/PositiveInfinity/NegativeInfinity` |
| `Draw.Sprite(id, x, y)`, `Draw.Text(x, y, text)` | 클라이언트 전용 렌더 API (그리기 목록은 호스트 엔진이 읽음, 1.11절) |
| `Net.View(x, y, r)`, `Net.ClearView()` | 클라이언트 전용: 이 클라이언트가 보는 원. 서버는 그 안의 엔티티(와 위치가 없는 엔티티)만 보냄 (5.8절) |
| `Print(...)` | 출력 (모든 컨텍스트) |
| `await NextTick()` / `Wait(초)` / `Until(조건)` | `async` 엔티티 메서드 안에서 실행을 다음 틱 이후로 미룸 (1.10절) |

`Spawn`, `Destroy`, `Print`, `Count`, `Query`와 같은 이름의 메서드를 선언해도 됩니다. 규칙은 C#의 멤버 조회와 같습니다: 그 클래스(파생 클래스 포함)나 struct 안에서 이름만 쓴 호출은 **선언한 메서드**로 갑니다. 다만 메서드가 받을 수 없는 호출 중 내장 함수의 형태인 것은 내장 함수로 갑니다. 인자(타입 인자) 개수가 다른 경우와 `Spawn(new 엔티티 { ... })`입니다(엔티티는 `Spawn`으로만 만들므로 어떤 메서드도 이 인자를 받을 수 없음).

```csharp
public int Count(ETeam t) { ... }       // 이 클래스 안에서:
int reds = Count(ETeam.Red);            // 선언한 메서드
int all = Count<Unit>();                // 내장 Count<T>() (메서드는 타입 인자를 받지 않음)
public static void Spawn(int n) { for (...) { Spawn(new Unit()); } }   // 안쪽은 내장 Spawn
```

둘 다 아니면(메서드의 인자 개수와도, 내장 함수의 형태와도 맞지 않으면) 메서드의 오류(E0241 "'Count' expects 1 argument(s)")가 납니다. 내장 함수를 같은 개수의 인자로 부르려다 자기 자신을 부르게 되면(`void Print(string s) { Print("> " + s); }`) E0312가 그 이유와 함께 메서드 이름을 바꾸라고 알려 줍니다. 다른 클래스에서는 `Log.Print(...)`처럼 클래스 이름을 붙여 부르므로 내장 함수와 겹치지 않습니다.

### 1.4.1 C#보다 엄격한 문장 규칙

C#에서는 경고나 코딩 표준으로만 다루는 실수 몇 가지가 Sijo에서는 문법 오류입니다. 같은 코드를 한 가지 방법으로만 쓰게 하려는 것입니다.

| 규칙 | 예 | 오류 | C# |
|---|---|---|---|
| `if`/`else`/`while`/`do`/`for`/`foreach`의 본문은 항상 블록 | `if (bDead) { return; }`. 빈 본문은 `{ }`. `else if`는 사슬을 이어 감 | E0031 | 문장 하나도 됨. `if (x);`는 경고 CS0642 |
| 선언 하나에 이름 하나 | `int a = 0;`과 `int b = 0;`을 따로 씀. 필드와 `for` 헤더도 같음(`for (int i = 0, j = n; ...)`은 오류) | E0030 | `int a, b;` 가능 |
| switch 문은 모든 값을 다룸 | 빠진 값이 있으면 `default:` (1.7절) | E0237 | 빠뜨려도 됨 |
| 불변 지역 변수는 선언할 때 값을 받음 | `int n = Count<Ship>();`. 나중에 대입하려면 `mut int n;` (1.6.2절) | E0310 | 나중에 대입해도 됨 |

### 1.4.2 메서드 오버로딩

C#처럼 한 이름에 매개변수가 다른 메서드를 여러 개 둘 수 있습니다(정적·struct·엔티티·인터페이스 메서드, 제네릭 struct·클래스·인터페이스의 인스턴스 안에서도).

```csharp
public static string Describe(int x) => $"int {x}";
public static string Describe(float x) => $"float {x}";
public static string Describe(Unit? u) => "unit";
public static string Describe(Orc? o) => "orc";         // Orc : Unit
public static bool Read(string s, out int v) => int.TryParse(s, out v);
public static bool Read(string s, out float v) => float.TryParse(s, out v);

Describe(3); Describe(2.5f); Describe('A') /* int: char → int이 char → float보다 나음 */; Describe(null) /* orc */;
Read("1.5", out float f);                                // out의 타입으로 고름
```

- **다른 점**은 매개변수의 타입과 모드(`ref`·`out`·`own`)입니다. 반환형·범위·`mut`만 다르면 같은 메서드를 두 번 선언한 것(E0104)입니다.
- **고르는 규칙**(C#의 더 나은 함수 멤버 규칙의 부분집합): 인자 개수가 맞고, `ref`/`out` 키워드가 같고(`ref` 인자는 타입도 정확히 같아야 함), 모든 인자가 매개변수 타입으로 암시적으로 바뀌는 오버로드 중에서, 모든 인자의 변환이 다른 후보보다 나쁘지 않고 하나라도 더 나은 것을 고릅니다. 같은 타입 > `char → int` > `int → float`·`char → float`, 핸들은 기반 클래스로 올라가는 단계가 적을수록, `null`은 가장 많이 파생된 클래스가 낫습니다. 맞는 것이 없거나 가장 나은 것이 하나가 아니면 E0267이 오버로드 목록과 함께 알려 줍니다(모호할 때는 `(float)x`처럼 인자를 바꾸라는 노트). 람다와 `default`, `out var x`는 어느 매개변수에나 맞는 것으로 봅니다.
- **가상 메서드**: `override`는 이름과 매개변수 타입이 같은 기반 클래스의 메서드를 재정의하고(없거나 반환형 등이 다르면 E0220), 매개변수가 다르고 `override`가 없는 메서드는 새 오버로드입니다. 디스패치(아키타입별 정적 디스패치)도 이름과 매개변수 타입으로 찾으므로 `Orc`가 `Worth(int)`만 재정의하면 `Worth(string)`은 기반 클래스의 것이 돕니다. 원격 호출(`other.Hit(2, 4)`)은 고른 오버로드의 지연 메시지가 됩니다.
- **인터페이스**: 인터페이스도 오버로드를 선언할 수 있고(기본 구현이 다른 오버로드를 불러도 됨), 구현은 매개변수 타입으로 짝을 짓습니다. 그래서 메서드가 `T`를 받는 같은 제네릭 인터페이스의 두 인스턴스를 한 클래스가 구현할 수 있습니다(`class Crate : Entity, IContainer<int>, IContainer<string>`이 `Put(int)`와 `Put(string)`을 가짐, `Query<IContainer<int>>()`의 `c.Put(1)`은 `Put(int)`로). 두 인스턴스에서 매개변수까지 같고 반환형만 다른 메서드(`T Peek()`)는 오버로드로 구별되지 않으므로 여전히 E0261입니다(C#의 명시적 인터페이스 구현에 해당하는 것은 없음).
- **오버로드할 수 없는 것**(E0267): 속성(이름 하나에 멤버 하나), `async` 메서드(코루틴 프레임이 메서드 이름에 붙음), `extern` 함수(C에는 오버로딩이 없음), 시스템 메서드 `Update(float dt)`와 `Game.Start()`. 제네릭 메서드와 같은 이름의 메서드도 둘 수 없습니다(E0104). 생성자는 전처럼 매개변수로 고르고, 연산자는 원래 오버로딩입니다.

## 1.5 매개변수 전달 규약 (C# 키워드 재활용)

| 표기 | Sijo 의미 | Rust 대응 |
|---|---|---|
| `T x` (기본) | **읽기 전용 빌림**. 작은 struct(≤16바이트)는 값으로, 큰 값/리스트는 포인터로 전달 — 의미는 같고 비용만 다름 | `&T` 또는 `T: Copy` |
| `mut T x` | 함수 안에서 바꿀 수 있는 복사본. 호출한 쪽 변수는 그대로입니다. 숫자·bool·enum·struct·문자열·핸들에 쓰고, 리스트를 바꾸려면 `mut own`이나 `ref`를 씁니다 | `mut x: T` |
| `own T xs` | 리스트·배열·사전을 **넘겨받음**. 호출한 쪽 변수는 호출로 이동하고(호출 지점에는 표시가 없음), 그 뒤에 쓰면 오류(E0501). 필드는 넘길 수 없음(E0502: `.Clone()`을 넘김). 2.4절 | `xs: Vec<T>` (이동) |
| `mut own T xs` | 넘겨받은 컬렉션을 바꿀 수 있음 | `mut xs: Vec<T>` |
| `ref T x` | 배타적 변경 빌림. 호출 지점에서 `ref` 필수 | `&mut T` |
| `out T x` | 쓰기 전용, 반환 전 반드시 대입. 호출 쪽은 `F(out v)`, 또는 C#처럼 그 자리에서 선언 `F(out int v)`·`F(out var v)`·`F(out _)`. 범위와 확정 대입은 C#과 같습니다: `if` 조건이나 식 문장에서 선언하면 바깥 블록에서 계속 쓸 수 있고, `while`/`for` 조건에서 선언하면 그 반복문 안에서만 보입니다 | `&mut MaybeUninit<T>` |

- **반환**: 리스트·배열·사전을 돌려주는 메서드와 속성은 반환 타입에 `own`을 씁니다: `public static own List<int> Evens(List<int> xs)`. 받은 쪽이 그 컬렉션을 소유하고 해제합니다. 빠뜨리면 E0505가 알려 줍니다.
- **다른 엔티티에 보내는 메시지**(시스템 끝에 적용되는 지연 호출, 4.9절)는 리스트·배열(다차원 포함)을 **복사해서** 가져갑니다. 원소는 값·문자열·struct·핸들·다른 리스트이고, 보내는 순간의 내용이 명령 버퍼에 담겨 받는 쪽은 자기 복사본을 받습니다. 기본 매개변수면 그 호출 동안 쓰고(프레임 아레나), `own`이면 받는 메서드의 것이 되어 필드에 둘 수 있습니다(`void Keep(own List<int> xs) { Kept = xs; }`). `own`으로 보내면 보낸 쪽 변수는 여느 `own` 호출처럼 이동합니다. 사전과 `ref`/`out`은 메시지로 보낼 수 없고(E0248), 메시지 하나의 문자열·리스트는 모두 합쳐 64KB까지입니다. 클라이언트 → 서버 RPC의 인자는 값과 문자열만 됩니다(E0608).
- C#의 `in` 매개변수·인자는 없습니다(E0028). 기본 매개변수가 이미 읽기 전용 빌림입니다. 확장 메서드(`this` 매개변수)도 없습니다.

참조(`ref`, `out`, 기본 매개변수의 빌림)는 **2급(second-class)** 입니다: 필드에 저장하거나 반환할 수 없습니다. 그래서 수명 주석 없이도 댕글링 참조가 원천적으로 불가능합니다 (C#의 ref safety 규칙, Hylo의 mutable value semantics와 같은 계열). 컬렉션을 오래 두려면 `own`으로 넘겨받습니다.

## 1.6 타입·수식어·특성 — 하나의 원칙

선언에 붙는 정보는 세 가지로 나뉘고, 어디에 둘지는 원칙 하나로 정합니다.

- **타입은 값이 무엇인지** 말합니다: null일 수 있는지(`T?`), 어떤 범위에 있는지(`int in 0..100`), 누가 소유하는지(`own`).
- **수식어는 코드가 무엇을 하고 어디서 도는지** 말합니다: 바뀌는지(`mut`), 클라이언트에서 도는지(`client`), 호스트가 구현하는지(`extern`), 코루틴인지(`async`), 그리고 `static`, `public` 등.
- **특성(`[...]`)은 컴파일러의 계획기에 주는 힌트이거나, 컴파일러가 검사하는 표시**입니다. 특성 하나를 지워도 서버가 시뮬레이션하는 결과는 바뀌지 않습니다. 바뀌는 것은 성능, 메모리 배치, 클라이언트에 보내는 것, 그리고 검사뿐입니다.

의미를 바꾸는 것은 특성으로 쓰지 않습니다. C#·Unity에서 익숙한 `[Range]`, `[Client]`, `[Server]`, `[DllImport]`를 쓰면 오류 E0029가 Sijo 표기(범위 타입, `client`, `extern`)를 알려 주고, 모르는 특성이나 맞지 않는 자리의 특성도 E0029입니다.

**타입**

| 표기 | 자리 | 의미 | C#과 다른 점 |
|---|---|---|---|
| `T?` | 엔티티 핸들 | 대상이 없을 수 있는 핸들. 쓰기 전에 `is T x`로 검사(2.2절). 필드에 담는 핸들은 반드시 `T?`(E0302) | C#은 참조 타입이 모두 null일 수 있음. Sijo는 핸들만 null이고 문자열·리스트에는 null이 없음. null 가능성은 타입이 말하므로 이름에 `OrNull` 같은 접미사를 붙이지 않음 |
| `T in lo..hi` | 엔티티 필드, 지역 변수, 값 매개변수, 반환 타입 | 범위 타입. 모든 쓰기가 범위로 포화되고, 복제할 때 비트 수가 정해짐(1.6.1절) | C#에 없음(Unity의 `[Range(a, b)]`는 인스펙터 표시일 뿐). C#의 `0..100`(인덱스 범위)은 끝을 빼지만 범위 타입은 **양 끝을 포함** |
| `own T` | 매개변수, 반환 타입 (리스트·배열·사전) | 컬렉션의 소유권이 넘어감(1.5절, 2.4절) | C#에 없음 (GC가 공유 참조를 관리) |

**수식어**

| 수식어 | 자리 | 의미 | C#과 다른 점 |
|---|---|---|---|
| `mut` | 지역 변수, 매개변수, 엔티티 필드, struct 메서드, 인터페이스의 기본 구현(struct가 받는 사본, 1.15절) | 초기화 뒤에도 바뀜. 없으면 바뀌지 않음(1.6.2절) | C#은 반대로 모두 바뀌고 `readonly`로 막음 |
| `client` | 엔티티 메서드, 정적 메서드 | 클라이언트에서 실행. 시뮬레이션 상태를 쓸 수 없고(E0601), 읽는 필드는 자동 복제(5장). 없으면 서버 코드 | C#에 없음. 서버 코드가 기본이라 `[Server]` 같은 표시는 없음 |
| `extern` / `extern("c_name")` | 정적 클래스의 `static` 메서드 | 호스트 프로그램이 C로 구현하는 함수(1.11절) | C#의 `[DllImport] static extern`에 해당 |
| `async` | 엔티티 메서드 | 코루틴(1.10절) | `Task` 대신 상태를 엔티티의 숨은 필드에 둠. `async void`만 |
| `public` / `private` / `protected` | 멤버 | C#과 같음 | `internal` 없음 |
| `static`, `const` | 멤버 | C#과 같음. `const`는 어떤 타입이든 됨(1.2절) | `readonly`·`static readonly` 없음 |
| `virtual` / `override` / `abstract` | 엔티티 클래스·메서드 | C#과 같음. 호출은 아키타입별 정적 디스패치 | |
| `ref` / `out` | 매개변수 | C#과 같음(1.5절) | `in` 없음 |

C#의 `readonly`, `internal`, `sealed`, `partial`, `unsafe`, 멤버를 가리는 `new`, 확장 메서드의 `this` 매개변수, `in` 매개변수는 Sijo에 없습니다. 쓰면 오류 E0028이 대신 쓸 것을 알려 줍니다. 아무 일도 하지 않는 수식어를 조용히 받아들이지 않기 위해서입니다.

수식어는 순서에 상관없이 쓰지만 같은 것을 두 번 쓰거나 서로 맞지 않게 쓰면 E0028입니다: 접근 수준 둘, `virtual`과 `override`/`abstract`, C 이름이 둘인 `extern`. 매개변수에서는 `mut own`과 `own mut`이 같고, `ref`/`out`에는 `mut`을 쓰지 않으며(이미 호출한 쪽 변수를 씀), `ref`·`out`·`own`은 하나만 씁니다. `mut`, `own`, `client`는 문맥 키워드라 타입 이름으로도 쓸 수 있습니다(뒤에 이름과 `;` `=` `,` `)` `(` `=>`가 오면 타입). 키워드와 기본 타입 이름(`in`, `foreach`, `int` 등)은 변수·필드·메서드·타입의 이름이 될 수 없습니다(E0011).

**특성**

| 특성 | 대상 | 의미 | 지우면 바뀌는 것 |
|---|---|---|---|
| `[Precision(p)]` | float 필드, float 멤버를 가진 struct 필드(멤버마다) | 복제 양자화 단계(기본 0.01). 범위와 함께 비트 수를 정함(5.3절). 범위가 없으면 효과가 없어 경고 W0203 | 대역폭 |
| `[Replicated]` / `[NotReplicated]` | 필드 | 복제 추론을 덮어씀(5.2절) | 클라이언트에 보내는 필드 |
| `[Spatial]` | float `X`·`Y`를 가진 struct 필드 | 샤딩·관심 영역의 기준 좌표(5.7절) | 샤드 배정, 시야 판정 |
| `[Component("Name")]` | 필드 | 컴포넌트 클러스터링을 덮어씀(4.3절) | 메모리 배치 |
| `[Pure]` | 메서드 | 순수 함수 계약(컴파일러가 검증). `extern` 함수에 붙이면 병렬 실행 허용 | 검사, `extern` 함수를 부르는 시스템의 병렬 실행 |
| `[Recursive]` | 메서드 | 스스로를 부를 수 있는 함수라는 표시. 컴파일러가 호출 그래프로 검사(1.6.3절) | 컴파일 오류(E0312) |
| `[Persistent]` / `[PersistentKey]` | 엔티티 클래스의 필드 / `string` 필드 하나 | 실행을 넘어 남는 값과 그 키(5.7.11절). `--store DIR`로 실행할 때만 저장·불러오기 | 다음 실행이 이어받는 값 |

특성은 위의 대상에만 붙습니다. 상수·struct 멤버·매개변수에 붙이거나, 같은 특성을 두 번 쓰거나, `[Replicated]`와 `[NotReplicated]`를 함께 쓰거나, 인자를 잘못 주면(`[Pure]`·`[Recursive]`·`[Spatial]`·`[Replicated]`·`[NotReplicated]`·`[Persistent]`·`[PersistentKey]`는 인자 없음, `[Precision]`은 양수 하나, `[Component]`는 문자열 하나) E0029입니다.

### 1.6.1 범위 타입 `T in lo..hi`

```csharp
public class Ship : Entity
{
    public mut int in 0..100 Hp = 100;                  // 양 끝 포함: 101개 값 → 7비트
    [Precision(0.01)] public mut SVec2 in 0..64 Pos;    // struct 필드: 모든 멤버(X, Y)가 0..64
    public mut float in -1..1 Steer;

    public void Hit(int in 0..50 damage)                // 값 매개변수: 인자가 0..50으로 포화
    {
        Hp -= damage;                                   // 쓰기마다 포화: 0 아래로 내려가지 않음
    }
}

public static class Rules
{
    public static float in 0..1 Ratio(float a, float b) => a / b;   // 반환 값도 포화 (NaN → 0)
}
```

- **자리**: 엔티티 필드(int, float, 숫자만 담은 struct — 범위가 모든 멤버에 적용), 지역 변수, 값 매개변수, 반환 타입(int, float), `extern` 함수의 매개변수와 반환 값(호출하는 쪽에서 포화하므로 호스트가 계약을 깰 수 없음). struct 멤버(E0224: 그 struct를 담는 엔티티 필드에 범위를 둠), 상수, `ref`/`out` 매개변수(가리키는 변수의 범위를 따름), `foreach` 변수(원소를 그대로 읽음; 범위가 필요하면 안에서 범위 있는 지역 변수에 담음), 컬렉션의 원소(`List<int in 0..9>`)에는 쓸 수 없습니다.
- 범위는 타입 바로 뒤에 하나만 씁니다. `in (0..5)`, `0..1..2`, 범위 두 개는 E0224이고, `0...5`는 E0004입니다.
- **경계**는 숫자이거나 상수(`MAX_HP`, `Rules.MAX_HP`, `-Rules.ARENA`)이고 음수를 쓸 수 있습니다. 상수는 숫자와 다른 상수로 계산되어야 합니다(`const int CAP = Rules.MAX_HP + 20;`는 되고, 함수를 부르는 상수는 E0224). 작은 쪽을 먼저 씁니다(`10..0`은 E0224). **양 끝을 포함**합니다. int 범위는 32비트 int 안에 있어야 합니다.
- **모든 쓰기가 포화합니다**: 대입, 복합 대입, `++`/`--`(식의 값도 포화된 값), 필드 기본값과 `Spawn` 초기화, 원격 명령 하나를 적용할 때마다(4.9절: 명령마다 포화하므로 명령의 순서가 결과를 정하고, 그 순서는 고정이라 결과는 결정론적), 범위가 있는 매개변수로 넘기는 인자, 반환 값, `TryParse`·`TryGetValue`의 `out` 쓰기, 저장 상태를 불러올 때(범위가 좁아졌으면 포화하고 `saturated:Ship.Hp`로 알림). `await`를 넘어 사는 지역 변수도 범위를 유지합니다. 컴파일 타임 계산(CTFE)도 같은 규칙을 따르므로 상수 인자로 부른 결과와 실행 결과가 같습니다.
- **`ref`/`out`으로 넘긴 변수는 호출이 끝날 때 포화합니다.** 호출된 함수의 매개변수에는 범위가 없으므로 그 안에서는 범위 밖 값을 잠시 가질 수 있습니다(호출된 함수는 호출한 쪽 변수의 범위를 모름).
- 그래서 위의 지점에서 읽은 값은 언제나 범위 안에 있고, 네트워크 계획기는 그 사실로 비트 수를 정합니다(5.3절).
- float의 NaN은 아래 끝(`lo`)이 됩니다. 범위 밖의 필드 기본값은 경고 W0202와 함께 포화된 값으로 시작합니다. 초기값을 쓰지 않은 필드는 0을 포화한 값으로 시작합니다(`int in 1..10 Lvl;`은 1, W0202가 알려 줌).
- 범위를 쓰지 않은 필드도, 모든 쓰기가 상수이거나 `Math.Clamp(e, a, b)`이면 컴파일러가 범위를 추론합니다(5.3절). 범위 타입은 그 범위를 계약으로 적는 방법입니다.

### 1.6.2 불변이 기본: `mut`

지역 변수, 매개변수, 엔티티 필드는 `mut`으로 선언했을 때만 초기화 뒤에 바뀝니다. 대부분의 값은 바뀌지 않으므로 바뀌는 쪽에 표시를 둡니다(Rust의 `let`/`let mut`과 같은 방향).

```csharp
public struct SVec2
{
    public float X;                       // struct 멤버에는 mut이 없음
    public float Y;

    public mut void Scale(float k)        // this를 바꾸는 struct 메서드는 mut
    {
        X *= k;
        Y *= k;
    }
}

public class Ship : Entity
{
    public ETeam Side;                    // 생성할 때 정하고 바뀌지 않음: Spawn(new Ship { Side = ETeam.Red })
    public mut SVec2 Pos;                 // 바뀌는 필드
    public mut List<SVec2> Trail;         // Add·Clear·xs[i] = v도 바꾸는 것

    public void Update(float dt)
    {
        int limit = 3;                    // 불변: 다시 대입하면 E0310
        mut float best = 1000000;         // 바뀌는 지역 변수
        mut var near = new List<Ship>();  // near.Add(...)를 하려면 mut
        for (int i = 0; i < limit; i++)   // for 헤더의 변수는 증감식에서만 바뀜
        {
            Pos.Scale(0.5);               // mut 메서드는 mut 자리에서만 부를 수 있음
        }
        foreach (var s in Query<Ship>())  // foreach 변수는 바뀌지 않음
        {
            float distSq = (s.Pos.X - Pos.X) * (s.Pos.X - Pos.X);
            if (s != this && distSq < best)
            {
                best = distSq;
                near.Add(s);
            }
        }
        Trail.Add(Pos);
    }
}
```

- 불변 지역 변수는 선언할 때 값을 받아야 합니다(E0310). 나중에 대입하려면 `mut int x;`로 선언합니다.
- 불변 엔티티 필드는 필드 기본값과 `Spawn(new T { F = ... })`의 초기화로만 값을 받습니다. 그 뒤의 쓰기는 자기 쓰기든 원격 쓰기든 E0310입니다. 바뀌지 않는 필드는 생성할 때 복제된 뒤로 변경을 보낼 일이 없고, 해저드 이중 버퍼(4.6절)가 되지 않습니다.
- 리스트·배열·사전을 바꾸는 연산(`Add`, `Remove`, `Clear`, `Sort`, `xs[i] = v`, `d[k] = v`, `Array.Sort(xs)` 등)도 바꾸는 것이므로 `mut` 바인딩이 필요합니다.
- struct 멤버에는 `mut`을 쓰지 않습니다(E0028). 멤버는 그 struct를 담은 변수나 필드가 `mut`일 때 바뀝니다(`mut SVec2 v`, `public mut SVec2 Pos`). `this`를 바꾸는 struct 메서드는 `mut`으로 선언하고, 바뀔 수 있는 자리(`mut` 변수·필드, `ref` 매개변수)에서만 부릅니다(E0403). 생성자는 멤버에 바로 대입합니다.
- 매개변수: `mut int n`은 함수 안의 복사본을 바꿉니다. `ref`/`out`에는 `mut`을 쓰지 않습니다(이미 호출한 쪽 변수를 씀). 빌린 컬렉션은 바꿀 수 없으므로 `mut own`이나 `ref`로 받습니다(1.5절). async 메서드의 매개변수는 항상 읽기 전용입니다.
- `for (int i = 0; i < n; i++)`의 헤더 변수는 증감식에서만 바뀝니다. 본문에서도 바꾸려면 `for (mut int i = 0; ...)`로 씁니다.
- `foreach` 변수는 바뀌지 않습니다(리스트 원소라면 읽기 전용 복사본, E0405). 쿼리 커서로 다른 엔티티의 `mut` 필드에 쓰는 것(`s.Hp -= 1`)은 됩니다(지연 명령).
- 그 자리에서 선언한 `out int v`·`out var v`는 호출이 초기화하는 불변 변수입니다. 나중에 바꾸려면 `mut int v;`를 먼저 선언하고 `out v`로 넘깁니다.
- `mut`으로 선언했는데 한 번도 바뀌지 않으면 린트 L316이 알려 줍니다(10.1절).

C#은 반대입니다. 모든 지역 변수·필드·매개변수가 바뀌고, 막고 싶을 때 `readonly`를 씁니다. Sijo에는 `readonly`가 없습니다(E0028).

### 1.6.3 `[Recursive]`

스스로를 부를 수 있는 함수에는 `[Recursive]`를 붙입니다. 다른 함수를 거쳐 돌아오는 상호 재귀도 포함합니다. 재귀 한 단계는 C 스택 프레임 하나를 쓰므로, 그 비용을 선언에서 보이게 합니다.

```csharp
[Recursive] public static int Fib(int n) => n < 2 ? n : Fib(n - 1) + Fib(n - 2);

[Recursive] public static bool IsEven(int n) => n == 0 || IsOdd(n - 1);   // 상호 재귀: 둘 다 표시
[Recursive] public static bool IsOdd(int n) => n != 0 && IsEven(n - 1);
```

- 컴파일러가 호출 그래프로 검사합니다. 엔티티 메서드 호출은 그 메서드를 재정의한 메서드까지 따라갑니다. 재귀하는데 표시가 없으면 오류 E0312(`'IsEven' calls itself through 'IsOdd': mark it [Recursive]`), 재귀하지 않는데 표시가 있으면 경고 W0312입니다.
- 스택에서 실행되는 호출만 셉니다. `base.M()` 호출, 다른 엔티티에 보내는 메시지(시스템 끝에 적용되는 지연 명령), 클라이언트의 자동 RPC는 재귀로 치지 않습니다.
- 그래서 이름에 `Recursive` 같은 접미사를 붙이지 않습니다. 재귀인지는 컴파일러가 확인하는 표시가 말합니다.

## 1.7 `switch`와 패턴 (C# 8~9)

```csharp
public static ERank Rank(int hp) => hp switch     // switch 식
{
    <= 0 => ERank.Dead,
    < 30 => ERank.Low,                            // 관계 패턴
    >= 30 and < 80 => ERank.Mid,                  // and / or / not
    _ => ERank.High,                              // 버림(discard)
};

switch (cmd)                                      // switch 문: 여러 레이블, when 가드
{
    case "quit":
    case "exit":
        return;
    case "fire" when bArmed:                      // when 가드
        Fire();
        break;
    default:
        Print("unknown");
        break;
}
if (team is ETeam.Red or ETeam.Blue) { ... }      // is 패턴
```

- 대상 타입: int, float, bool, enum, string, 엔티티 핸들(`null`/`not null`만).
- **완전성 검사**: switch 식과 switch 문은 모두 모든 값을 다뤄야 합니다(E0237). 나머지 값은 switch 식에서 `_`, switch 문에서 `default:`가 맡습니다. int·enum·bool은 구간 집합 분석으로 판단하므로 `< 0`, `0`, `> 0` 세 갈래나 enum 값을 모두 쓴 switch 문은 `default:` 없이도 완전합니다. float과 string은 `_`/`default:`가 필요합니다. C#은 switch 문이 값을 빠뜨려도 되고, switch 식은 경고(CS8509)를 내고 실행 중에 예외를 던집니다.
- **도달 불가 검사**: 앞 갈래가 이미 처리한 값만 다루는 패턴, 아무 값도 맞지 않는 패턴(`> 5 and < 3`), 중복 case 값은 오류(E0238).
- switch 문은 C#처럼 다음 section으로 흘러내릴 수 없습니다(E0236). `break`는 switch를, `continue`는 바깥 루프를 대상으로 합니다.

## 1.8 문자열

```csharp
mut string name = $"Ship {id}";              // 보간 문자열은 값 (다시 대입하므로 mut)
name += "!";                                 // 숫자·bool·enum은 자동 ToString
if (name.StartsWith("Ship") && name.Length > 4) { ... }
string tag = name.Substring(0, 4).ToUpper();
int n = int.Parse("42");
string csv = string.Join(", ", names);       // List<string>/List<int> 등
switch (cmd) { case "fire": Fire(); break; default: break; }   // 문자열 switch (default: 필요)
```

- 불변 UTF-8 값입니다. `Length`·인덱스는 **문자 수**를 셉니다(한글도 글자 하나가 1, BMP 범위에서 C#과 같음). ASCII만 담은 문자열은 그 사실을 기억해 두므로 `s[i]`·`Substring`이 O(1)입니다. 그 밖의 문자열은 UTF-8을 앞에서부터 셉니다(`foreach`는 어느 쪽이든 한 번에 훑음).
- `char`: 유니코드 코드 포인트 하나(`'a'`, `'\n'`, `'한'`). `s[i]`는 `char`, `foreach (char c in s)`, 산술(`c - '0'`, `(char)(c + 1)`), 비교, `switch`, 사전 키. `char.IsDigit/IsLetter/IsLetterOrDigit/IsWhiteSpace/IsUpper/IsLower/IsPunctuation/ToUpper/ToLower`는 .NET의 유니코드 분류·대소문자 표를 따릅니다(로마자·그리스·키릴·한글 등; 표는 컴파일러가 생성 코드에 넣음).
- 메서드: `Length`, `s[i]`, `Substring(i[, n])`, `Contains`, `StartsWith`, `EndsWith`, `IndexOf`, `LastIndexOf`(문자열·`char`), `Replace`, `Trim`, `ToUpper`, `ToLower`(유니코드), `PadLeft/PadRight(n[, c])`, `Split(c)`, `CompareTo`, `Equals`; 생성: `new string(c, n)`; 정적: `string.IsNullOrEmpty/Join/Concat/Compare`, `int.Parse`, `float.Parse`, `int.TryParse(s, out v)`, `float.TryParse`; `x.ToString()`.
- 이스케이프는 C#과 같습니다: `\n \t \r \0 \a \b \f \v \\ \" \'`, `\xH`(1~4자리), `\uHHHH`, `\UHHHHHHHH`. 서로게이트 쌍(`\uD83D\uDE00`)은 한 글자로 합쳐지고, 짝 없는 반쪽(`\uD800`)과 잘못된 이스케이프는 오류(E0005)입니다.
- `null` 문자열은 없습니다. 기본값은 `""`입니다.
- 메모리 관리는 3.8절(틱 경계 수집기)을 보세요. 문자열은 다른 엔티티에 보내는 메시지(`other.SetName(s)`)와 RPC의 인자로 쓸 수 있습니다(길이 + 바이트로 직렬화, 서버가 UTF-8과 크기를 검증). 클라이언트 코드가 읽는 문자열 필드는 다른 필드처럼 **복제**됩니다(5.3절). 문자열을 담은 struct를 RPC 인자로 보낼 수는 없습니다(E0249: 문자열 매개변수로 나눠서 보냄).

## 1.9 `Dictionary<K, V>`

```csharp
public mut Dictionary<EItem, int> Bag;       // 엔티티 소유: 엔티티와 함께 해제. 바꾸려면 mut
Bag[EItem.Gold] += 10;                       // 없는 키 읽기는 패닉 (C#의 KeyNotFoundException)
mut int potions;                             // out으로 나중에 받으므로 mut
if (Bag.TryGetValue(EItem.Potion, out potions)) { ... }  // 미리 선언한 변수, 또는
if (Bag.TryGetValue(EItem.Gold, out int gold) && gold > 10) { ... }  // out int x / out var x / out _
foreach (var kv in Bag) { Print($"{kv.Key}={kv.Value}"); }
foreach (var k in Bag.Keys) { ... }          // .Values도 가능
```

- 키: int, enum, string, 엔티티 핸들. 값: 리스트·사전이 아닌 모든 값 타입(문자열을 담은 struct 포함).
- 연산: 인덱서 읽기/쓰기, `d[k] op= v`, `Add`(중복 키는 패닉), `ContainsKey`, `Remove`(O(1)), `Clear`, `TryGetValue`, `GetValueOrDefault(k[, 기본값])`, `Count`.
- 순회 순서는 삽입 순서이며, `Remove`는 마지막 항목을 빈자리로 옮깁니다(결정론적).
- 리스트와 같은 소유 규칙을 따릅니다: 지역 사전은 프레임 아레나, 필드는 엔티티 소유, 다른 엔티티의 사전은 직접 수정할 수 없고, 순회 중 수정은 오류(E0406)입니다.

## 1.10 코루틴: `async` / `await`

```csharp
public class Director : Entity
{
    public mut int Wave;

    public async void Run()                  // 엔티티가 생성되면 시작, 매 틱 이어서 실행
    {
        while (Wave < 10)
        {
            Wave += 1;
            for (int i = 0; i < Wave * 3; i++)
            {
                Spawn(new Enemy { ... });
                await Wait(0.5);             // 0.5초(게임 시간) 뒤에 이어서
            }
            await Until(Count<Enemy>() == 0);    // 조건이 참이 될 때까지
            await NextTick();
        }
    }
}
```

- `async void M(...)`은 가상이 아닌 엔티티 메서드여야 합니다(E0801). 매개변수는 값(리스트, non-nullable 핸들, `ref`/`out`/`own` 제외)이고 읽기 전용입니다. 전역 스크립트는 `Game.Start`에서 생성하는 "감독" 엔티티에 둡니다.
- 매개변수 없는 async 메서드는 엔티티가 생기면 저절로 시작합니다. 다른 async 메서드는 `await M(args)`로 부르며, 부른 쪽이 매 틱 이어서 실행하다가 끝나면 다음 줄로 넘어갑니다(인자는 await가 시작할 때 한 번 계산되어 숨은 필드에 저장됨). 서로를 기다리는 순환은 오류입니다(E0804: 코루틴 프레임은 엔티티마다 하나).
- **구조적 동시성**: `int w = await Race(MoveTo(b), Wait(5), Until(Hp < 10));`는 갈래들을 함께 돌려 먼저 끝난 갈래의 번호(0부터)를 돌려주고 나머지는 취소합니다. `await Sync(Blink(2), MoveTo(b));`는 모든 갈래가 끝날 때까지 기다립니다. 갈래는 이 엔티티의 async 메서드, `Wait(초)`, `Until(조건)`, `NextTick()`이며(2~16개), 매 틱 적힌 순서대로 실행되므로 같은 틱에 여럿이 끝나면 앞 갈래가 이깁니다. 시간은 혼자 쓴 `await`와 같게 셉니다. 한 메서드를 두 갈래에 쓸 수는 없습니다(E0804). 취소된 메서드는 다음에 기다릴 때 처음부터 시작합니다. `int w = await Rush(Patrol(), Wait(3));`는 `Race`처럼 먼저 끝난 갈래에서 이어 가지만, 아직 끝나지 않은 메서드 갈래를 취소하지 않고 **배경에서 계속 돌립니다**(매 틱 한 단계씩, 끝날 때까지). 배경에서 도는 메서드를 다시 `await`하면 그 메서드는 처음부터 다시 시작해 앞에서 돕니다. 배경 갈래는 코루틴의 시스템이 돌려 주므로, `Rush`는 다른 코루틴이 기다리지 않는 매개변수 없는 async 메서드에서만 쓸 수 있습니다(E0805).
- 컴파일러는 메서드를 상태 기계로 바꾸고 **재개 지점·타이머·await를 넘어 살아 있는 지역 변수를 숨은 엔티티 필드**에 둡니다. 그래서 코루틴은 엔티티마다 데이터 병렬로 돌고, 결정론적이며, 저장·복원과 라이브 코딩 재시작을 넘어 대기 중인 상태 그대로 이어집니다(4.11절).
- 안전 규칙(E0802): 살아 있음이 보장된 핸들(`Ship s`)이 await를 넘어 범위에 있으면 오류입니다 — 기다리는 동안 그 엔티티가 파괴될 수 있으므로 `Ship?`로 두고 await 뒤에 `is`로 다시 확인합니다. `foreach` 안의 await, `atomic` 안의 await도 오류입니다.

## 1.11 호스트 엔진과 연결: `extern`

```csharp
public static class Host
{
    public static extern("engine_key_down") bool IsKeyDown(EKey key);
    public static extern void PlaySound(string name, float volume);
}
```

- 호스트 프로그램이 C로 구현하는 함수입니다. `extern("c_name")`은 C 함수 이름을 정하고, 이름이 없으면 메서드 이름을 그대로 씁니다. C#의 `[DllImport("lib")] static extern`과 달리 라이브러리 이름은 없습니다(호스트와 함께 링크). 인자: int·float·bool·enum·string(`const char*` + 바이트 길이), int·float·bool·char·enum의 배열과 리스트(`const int32_t* cells, int32_t cells_len`: 호출하는 동안만 빌려주는 읽기 전용 포인터와 원소 수, 문자열과 같은 방식). 반환: void·int·float·bool·enum.
- C 이름은 C 키워드, `main`, 표준 C·POSIX 함수 이름(`printf`, `malloc` 등), `sijo_`·`Sijo`·`SIJO_`·`omp_`로 시작하는 이름, `__x`·`_X` 같은 C 예약 이름, 컴파일러가 만드는 이름과 겹칠 수 없습니다(E0254, 이유를 알려 줌).
- 범위가 있는 매개변수와 반환 값은 호출하는 쪽에서 포화하므로 호스트가 계약을 깰 수 없습니다(1.6.1절).
- 외부 효과로 취급되어 해당 시스템은 직렬 실행되고 `atomic` 안에서 쓸 수 없습니다(`[Pure]`를 붙이면 병렬 허용). `explain`은 직렬 이유로 그 extern 이름을 보여 주고, 린트 L409가 호출 지점을 알려 줍니다.
- **`[Deferred]`**: 반환값이 없는(void) extern에 붙이면 서버에서의 호출은 인자를 복사해 스레드별 명령 버퍼에 쌓았다가 스테이지 끝에 **명령 순서(행 순서)대로** 실제로 부릅니다. 그래서 흔들기·소리·알림처럼 결과를 기다리지 않는 호출이 시스템을 직렬로 만들지 않습니다. 호출 순서는 스레드 수와 샤드 수에 관계없이 직렬 실행과 같고(샤드에서는 코디네이터가 한 번만 부름), `atomic`이 `fail`하면 그 블록의 호출도 버려집니다. 클라이언트 코드에서는 바로 부릅니다. 직접 부르는 출력과의 순서를 지키려고 이 호출을 하는 시스템은 출력하는 시스템과 같은 스테이지에 놓이지 않습니다.

```csharp
public static class Host
{
    [Deferred] public static extern("host_shake") void Shake(float amount);    // Mob.Update가 병렬 그대로
    public static extern("host_draw_row") void DrawRow(int y, int[] cells);   // void host_draw_row(int32_t y, const int32_t* cells, int32_t cells_len)
}
```
- `sijoc build game --lib`은 `main` 없이 게임과 `sijo_game.h`(생성·한 틱 진행·그리기 목록·저장·복원 API, 엔티티 클래스의 public 필드를 읽는 함수)를 만들고, `--host host.c`는 호스트 프로그램을 함께 컴파일합니다. 예: [`examples/dodge`](../examples/dodge) (ncurses 터미널 게임). 자세한 사용법은 10.7절, 웹(WebAssembly)은 10.11절.

## 1.12 여러 파일로 된 프로젝트

디렉터리에 `sijo.toml`을 두면 그 아래의 모든 `.sijo` 파일이 하나의 프로그램이 됩니다. 파일 순서는 상관없습니다(타입·상수는 어느 파일에서든 참조 가능). 엔티티 시스템이 도는 순서는 기본이 선언 순서(여러 파일이면 파일 경로 순)이고, 파일 이름에 기대지 않고 정할 수 있습니다(4.9절).

```toml
name = "mygame"
order = ["Input", "*", "Render"]   # Input의 시스템 먼저, 나머지(*)는 원래 순서, Render는 마지막. "Mob.Think"처럼 시스템 하나도 됨
```

클래스에 `[Order(n)]`을 붙여도 됩니다(작은 수가 먼저, 기본 0, 같으면 원래 순서). `order` 목록이 먼저이고, 같은 자리 안에서 `[Order(n)]`, 그다음 원래 순서입니다. 목록에 없는 이름은 오류입니다. `sijoc build/run/check/lint/live <디렉터리 또는 프로젝트 안의 파일>`, 여러 파일을 나열해도 됩니다. 진단은 원래 파일과 줄을 가리키고, 언어 서버는 다른 파일로의 정의 이동·참조·이름 변경을 지원합니다.

## 1.13 숫자 서식·파싱과 수학 함수 — C#과 같은 결과

숫자를 문자열로 바꾸는 모든 경로(`Print`, 보간 문자열, `ToString()`, `+` 연결, `StringBuilder`식 추가)는 **.NET 8의 InvariantCulture 출력과 같은 문자열**을 만듭니다. 이 결과는 플랫폼과 C 라이브러리에 의존하지 않습니다(정확한 큰 수 십진 변환을 런타임에 직접 구현).

| 서식 | int | float | 예 |
|---|---|---|---|
| 기본 (`{x}`, `ToString()`) | 십진 | 왕복 가능한 가장 짧은 표현 | `0.1f` → `0.1`, `1e8f` → `100000000`, `1e10f` → `1E+10`, `0.00001f` → `1E-05`, `float.NaN` → `NaN` |
| `D`n | 자릿수 채움 | — | `{7:D3}` → `007` |
| `X`n / `x`n | 16진(2의 보수) | — | `{-1:X}` → `FFFFFFFF` |
| `F`n / `N`n | 고정 소수점 / 천 단위 구분 | 같음 | `{2.5f:F0}` → `2`, `{3.5f:F0}` → `4`(.NET 8처럼 정확한 값 기준, 동점은 짝수 쪽), `{1234.5f:N1}` → `1,234.5` |
| `E`n | 지수 표기(3자리 지수) | 같음 | `{1234.5f:E2}` → `1.23E+003` |
| `G` / `R` | 십진 | 가장 짧은 표현 | |

정밀도는 0~99입니다. 지원하지 않는 서식은 컴파일 오류(E0255)입니다.

파싱도 C#과 같습니다. `int.Parse`/`TryParse`는 앞뒤 공백과 부호를 허용하고 범위를 넘으면 실패합니다. `float.Parse`/`TryParse`는 소수점·지수·천 단위 쉼표·`NaN`·`Infinity`를 받아 **정확히 반올림한** 값을 냅니다. `Parse` 실패는 패닉, `TryParse` 실패는 `false`입니다.

`Math`의 float 함수(`Sin`, `Cos`, `Tan`, `Asin`, `Acos`, `Atan`, `Atan2`, `Exp`, `Log`, `Log10`, `Log2`, `Pow`)는 플랫폼의 libm을 쓰지 않고 IEEE 754 기본 연산만으로 계산하는 자체 구현입니다. 그래서 gcc·clang, x86-64·ARM64·빅엔디언 s390x, Linux·Windows, `-O0`~`-O3 -march=native`에서 결과가 비트 단위로 같고, 컴파일 타임 계산(CTFE)도 같은 알고리즘(Rust 이식)을 써서 런타임과 같은 값을 냅니다. 정밀도는 float 반올림 1회 이내입니다. 특수값(NaN, ±0, ±∞)은 C#의 `MathF`와 같고, `Math.Round`는 C#처럼 짝수 반올림, `Math.Sign(NaN)`은 C#처럼 패닉입니다.

## 1.14 배열·람다·제네릭

```csharp
mut int[] scores = new int[8];                // 기본값 8개 (참조 원소는 빈 리스트·빈 문자열·null 핸들)
string[] names = { "ann", "bob" };
Array.Sort(scores);                           // 배열을 바꾸므로 scores는 mut. Array.Reverse/IndexOf/Fill/Copy/Find/Exists/FindIndex/TrueForAll/ForEach
List<int>[] buckets = new List<int>[4];       // 원소는 빈 리스트로 시작 (C#은 null)

mut int[,] grid = new int[W, H];              // 다차원 배열 (T[,,]까지)
int[,] tiles = { { 1, 2, 3 }, { 4, 5, 6 } };  // 2행 3열: tiles[1, 2] == 6
for (int x = 0; x < grid.GetLength(0); x++)
{
    for (int y = 0; y < grid.GetLength(1); y++)
    {
        grid[x, y] = tiles[x % 2, y % 3];     // 첨자마다 자기 차원의 길이로 검사 (이 루프에서는 컴파일러가 증명해 제거)
    }
}
foreach (int t in grid) { ... }               // 행 우선 순서(C#과 같음): grid[0, 0], grid[0, 1], ...
var copy = grid.Clone();                      // int[,] (C#의 (int[,])grid.Clone()도 됨)

var alive = ships.Where(s => s.Hp > 0).OrderBy(s => s.Hp).ToList();
ships.Sort((a, b) => b.Score - a.Score);      // 안정 정렬 (ships는 mut 리스트)
int limit = 3;
log.RemoveAll(x => x > limit);                // 지역 변수·필드·매개변수를 캡처 (log는 mut 리스트)
units.ForEach(u => Print(u.Name));

public static T Max<T>(T a, T b) => a.CompareTo(b) >= 0 ? a : b;
Max(3, 7); Max(1, 2.5f) /* T = float */; Max<string>("a", "b");

public struct Pair<T>
{
    public T First;
    public T Second;
    public Pair(T a, T b) { First = a; Second = b; }
    public static Pair<T> Of(T v) => new Pair<T>(v, v);
}
var p = new Pair<int>(1, 2);
var q = Pair<string>.Of("x");

public class Pool<T> : Entity, IScore where T : struct   // 제네릭 엔티티 클래스
{
    public mut List<T> Items;
    public T Seed;
    public void Update(float dt) { Items.Add(Seed); }
    public void Put(T x) { Items.Add(x); }
    public virtual int Score() => Items.Count;
}
public class IntPool : Pool<int> { public override int Score() => base.Score() * 10; }
public class Hunter<T> : Entity where T : Unit { public mut T? Prey; ... }

Pool<int>? ints = Spawn(new Pool<int> { Seed = 5 });   // Pool<int>, Pool<SVec2>는 서로 다른 엔티티 클래스
foreach (var v in Query<Pool<SVec2>>()) { v.Put(new SVec2(1, 2)); }
if (s is Pool<int> pi) { ... }
int n = Count<Pool<int>>();                             // IntPool도 Pool<int>이므로 함께 셈
```

- **배열** `T[]`: 길이가 고정된 리스트입니다(`Length`, `Rank`, `GetLength(0)`, `Clone()`, 인덱서, `foreach`). 메모리 영역·소유·이동 규칙은 리스트와 같습니다(3장). 엔티티 필드의 배열은 상수 크기·상수 원소로 초기화할 수 있습니다(`public int[] Slots = new int[4];`; `new int[90000]`도 할당 한 번이고 원소마다 코드가 생기지 않음).
- **고정 길이 배열 필드** `public mut int[K.CELLS] Cells;`: 길이(상수)가 선언의 일부입니다. 엔티티가 생길 때 그 길이만큼 기본값으로 시작하고(초기화 식을 쓰면 길이가 같아야 함, E0241), 다른 길이의 배열을 대입하면 패닉, `own`·`ref`·`out`으로 넘기면 오류입니다(E0241). 그 대가로 컴파일러가 길이를 압니다: `for (int i = 0; i < K.CELLS; i++) Cells[i]`처럼 상수까지 도는 루프는 루프 앞에서 길이를 한 번 확인하고 원소마다의 경계 검사를 지웁니다(다른 엔티티의 필드도). 저장된 상태의 길이가 다르면(라이브 코딩 중에 길이를 바꿈) 선언된 길이로 다시 시작하고 `resized:`로 알립니다.
- **다차원 배열** `T[,]`·`T[,,]`: 원소를 C#과 같은 행 우선 순서로 한 버퍼에 담고 차원마다 길이를 함께 둡니다. `new T[w, h]`(원소는 기본값; 엔티티 핸들이면 `T?`), 초기화 `{ { 1, 2 }, { 3, 4 } }`(행 길이가 다르면 E0241, 길이를 함께 쓰면 상수이고 같아야 함), `a[i, j]`(첨자 개수는 차원 수, 아니면 E0239), `Length`(모든 원소 수), `Rank`, `GetLength(d)`, `GetUpperBound(d)`(상수 `d`가 차원 밖이면 E0241, 변수면 실행 중 패닉), `Clone()`, `foreach`(행 우선). 첨자는 **차원마다** 그 차원의 길이로 검사합니다: 3x4 배열의 `a[0, 4]`는 원소 수(12) 안이어도 패닉입니다. `for (x = 0; x < a.GetLength(0); x++)`처럼 그 차원의 길이까지 도는 루프의 첨자와, 길이가 상수로 알려진 배열(`new int[8, 8]`, 원소를 써도 길이는 그대로)의 범위 안 첨자는 검사를 지웁니다(7.3절). 지역 변수, 엔티티 필드(상수 길이·원소로 초기화, 엔티티가 파괴되면 해제, 다른 엔티티는 스냅샷으로 읽음, 클라이언트가 읽으면 길이와 원소를 복제), 매개변수(빌림·`ref`·`own`), 반환(`own int[,]`), 메시지 인자, 다른 컬렉션의 원소(`int[][,]`는 `int[,]`의 배열, `int[,][]`는 `int[]`의 2차원 배열: C#처럼 맨 앞 괄호가 바깥 배열)로 쓸 수 있습니다. 4차원 이상은 E0019입니다. C#처럼 LINQ·람다 메서드와 `Array.Sort` 같은 `Array` 함수는 1차원 배열에만 있습니다(E0101/E0241: 중첩 루프로).
- **람다**를 리스트·배열 메서드에 넘기면(`Sort(비교)`, `Find/FindLast/FindIndex/FindLastIndex`, `Exists`, `TrueForAll`, `RemoveAll`, `ForEach`, LINQ식 `Any/All/Count/Sum/Min/Max/First/Last/FirstOrDefault/LastOrDefault/Where/Select/OrderBy/OrderByDescending/ToList/ToArray`) 컴파일러가 **호출 지점에 루프로 펼치므로** 클로저 할당도 간접 호출도 없습니다. 순회 중인 리스트를 람다 안에서 바꾸거나 옮기면 오류입니다(E0406). 람다 본문은 원소마다 실행되므로 바깥 컬렉션을 그 안에서 옮길 수 없습니다(E0501). 람다를 변수·필드에 담거나 반환하려면 델리게이트 타입이 필요합니다(1.14.1절, 없으면 E0027).
- **제네릭 메서드**는 정적·struct·엔티티 메서드 어디에나 둘 수 있습니다. 타입 인자는 직접 쓰거나 인자에서 추론합니다(C#처럼: `int`와 `float`가 섞이면 `float`, 엔티티 핸들은 공통 기반 클래스). 추론할 수 없으면 E0259가 쓰는 방법을 알려 줍니다. 가상·async·`extern` 제네릭 메서드는 없습니다(E0258).
- **제네릭 struct**는 필드, 생성자, 연산자, 정적·인스턴스 메서드, 필드 초기화를 가질 수 있고, 인터페이스를 구현할 수 있으며(`struct SStack<T> : IContainer<T>`, 1.15절), 엔티티 필드나 제네릭 메서드의 매개변수(`T Left<T>(Pair<T> p)`)로도 씁니다.
- **제네릭 엔티티 클래스**(`class Pool<T> : Entity`)는 쓰인 타입 인자마다 **엔티티 클래스 하나**가 됩니다. `Pool<int>`와 `Pool<SVec2>`는 아키타입·컴포넌트·시스템(`Pool<int>.Update`)이 따로 있고, 죽은 필드 제거·복제 추론·해저드 탐지도 인스턴스마다 따로 합니다. 그래서 ECS·병렬 스케줄·네트워크 복제·저장 상태(`arch Pool<int> ...`)·`sijoc explain`·LSP 호버에는 `Pool<int>`라는 이름의 보통 클래스로 보입니다. 인스턴스끼리는 상속 관계가 없습니다(`Query<Pool<int>>()`는 `Pool<SVec2>`를 돌지 않음). 공통으로 다룰 것은 인터페이스나 제네릭이 아닌 기반 클래스로 묶습니다.
  - 필드와 메서드에 `T`를 씁니다: `T`, `List<T>`, `T[]`, `Dictionary<string, T>`, 엔티티 참조 `T?`(`where T : Entity`나 엔티티 클래스 제약이 필요하고, 없으면 그 인스턴스에서 E0210), 문자열 등 모든 필드 타입. 메서드는 보통 메서드·`virtual`/`override`/`abstract`·`client`·`async`·정적 메서드(`Pool<int>.Cap()`)·제네릭 메서드(`int Tally<C>(C x)`; 클래스의 타입 매개변수와 이름이 같으면 E0258)가 됩니다. 범위 타입·특성(`[Spatial]`, `[Replicated]` 등)도 그대로 씁니다.
  - 기반 클래스와 인터페이스: `class Tagged<T> : Base`, `class Pool<T> : Entity, IScore`, 다른 인스턴스를 기반으로 `class Layer<T> : Pair<T, string>`, 구체 타입 인자로 만든 인스턴스를 기반으로 `class IntPool : Pool<int>`(재정의는 `T`를 바꿔 넣은 시그니처와 같아야 함, E0220). 추상 제네릭 클래스도 됩니다.
  - 쓸 때는 항상 타입 인자를 씁니다. `Pool`만 쓰거나(`new Pool { }`, `Count<Pool>()`, `: Pool`, `x is Pool`) 개수가 틀리면 E0228이 예를 들어 알려 줍니다. `where` 제약은 쓰는 지점마다 검사합니다(E0260, `: Pool<string>`도). 타입 인자로 만들 수 없는 인스턴스는 그 인스턴스를 가리키는 노트와 함께 오류입니다: 엔티티 클래스를 `T`로 받아 `T` 필드에 담으면 E0302(`T?`로), `T?`에 `int`를 주면 E0210, 그 타입에 없는 연산은 E0239. 자기 자신을 점점 큰 타입 인자로 쓰는 클래스(`Node<List<T>>? Next`)는 인스턴스 64개에서 멈춥니다(E0259), `class C<T> : C<T>`는 상속 순환(E0214), 타입 매개변수에서 파생(`class C<T> : T`)은 E0209입니다.
  - 제네릭 엔티티 클래스에 둘 수 없는 것: 상수(E0258: 제네릭 정적 클래스나 제네릭이 아닌 기반 클래스에 둠), `extern` 메서드(E0258), 그리고 C#과 같이 생성자·연산자(E0217/E0218). 같은 실수는 인스턴스가 몇 개든 한 번만 알립니다.
- **제네릭 정적 클래스** `static class Pairs<T>`는 타입 인자마다 함수·상수 묶음 하나가 됩니다(`Pairs<int>.Swap(xs)`, `Limits<float>.Default`; 각 인스턴스는 제네릭 struct의 정적 메서드처럼 따로 검사·컴파일). C#의 정적 클래스와 같이 타입이 아니고(`Pairs<int> p`는 E0229), 쓸 때는 타입 인자를 씁니다(`Pairs.Swap`은 E0228). Sijo에는 변경 가능한 정적 필드가 없으므로(E0206) C#의 "타입 인자마다 따로 있는 정적 필드"는 없고, 상수는 인스턴스마다 그 타입으로 계산됩니다(`const T None = default(T);`). `extern`·생성자는 둘 수 없고, `Game`은 제네릭일 수 없습니다(E0258).
- **타입 매개변수의 정적 멤버**: 제네릭 코드 안의 `T.Zero()`, `T.Parse(s)`, `T.MaxValue`는 그 인스턴스의 타입 인자의 정적 멤버입니다(인스턴스마다 검사하므로 C# 11의 `static abstract` 인터페이스 멤버로 쓰는 제네릭 수학과 같은 일이 됨, 1.15절). `int`·`float`·`char`·`string`이면 그 내장 정적 멤버(`int.Parse`, `float.MaxValue`)입니다.
  - 생성되는 C 이름은 인스턴스 이름을 그대로 바꿔 씁니다(`a_Pool_x3c_int_x3e_`; 선언된 클래스 이름과 겹치면 인스턴스 쪽에 번호를 붙임). 아키타입은 템플릿을 선언한 자리 바로 뒤에, 처음 쓰인 순서대로 놓이므로 시스템 순서와 결과는 결정론적입니다. 아키타입은 프로그램 전체에서 64개까지입니다(8.4절).
- 검사 방식: 제네릭 코드는 **쓰인 타입 인자마다 따로** 타입 검사·컴파일됩니다. `T`에 없는 연산(예: struct에 `>`)을 쓰면 그 인스턴스에서 `in 'Max<SVec2>'` 노트와 함께 오류가 납니다. 한 번도 쓰이지 않은 제네릭 클래스·struct·메서드는 **타입 매개변수를 모르는 채로** 한 번 검사합니다: 타입 인자와 무관하게 틀린 것(알 수 없는 이름, 인자 개수, 알려진 타입끼리의 불일치, 반환하지 않는 경로, 불변 바인딩에 쓰기 등)을 `in 'Pool<T>', which nothing uses` 노트와 함께 알리고, `T`에 대한 연산(`x > b`, `V.Length`, `T.Zero()`)은 쓰는 인스턴스가 생길 때 검사합니다. (컴파일러는 쓰이지 않은 코드가 있을 때만 그것을 오류 타입으로 인스턴스화해 검사를 한 번 더 돌리고, 그 선언 안의 진단만 가져옵니다.) 자기 자신을 점점 큰 타입 인자로 부르는 재귀(`F<List<T>>`)는 인스턴스 64개에서 멈춥니다(E0259).
- **`where` 제약**은 인스턴스마다 타입 인자에 대해 검사합니다(E0260, 호출 지점·타입 사용 지점에 `type argument 'string' does not satisfy 'where T : struct' of 'Sum'`와 이유). `struct`(숫자·bool·`char`·enum·struct), `class`/`class?`(엔티티·문자열·리스트·사전·배열), `new()`(문자열·배열·추상 클래스 제외), `notnull`(nullable 엔티티 참조 제외), `unmanaged`(참조가 없는 값 타입), `Entity`(엔티티 클래스), 엔티티 클래스 이름(그 클래스나 파생 클래스)을 쓸 수 있습니다. 본문은 어차피 인스턴스마다 검사되므로 제약은 요구 사항을 드러내고 오류를 호출 지점에서 알려 주는 역할입니다. 타입 매개변수가 아닌 이름, 같은 매개변수의 `where` 두 번, `class`와 `struct`를 함께, struct·기본 타입을 제약으로 쓰는 것은 오류이고, Sijo에 없는 인터페이스(`IComparable<T>`)는 알 수 없는 타입(E0105)입니다.

### 1.14.1 델리게이트: 람다와 메서드를 값으로

```csharp
public delegate int Op(int a, int b);                    // 이름 있는 델리게이트 타입
public static int Apply(Op op, int a, int b) => op(a, b);
public static Func<int, int> Adder(int n) => x => x + n;  // n을 캡처한 람다를 반환
public static Func<int, int> Compose(Func<int, int> f, Func<int, int> g) => x => g(f(x));

public class Ship : Entity
{
    public mut int Hp = 100;
    public Func<int, int> Double = x => x * 2;           // 캡처가 없는 람다는 상수: 필드 초기값이 됨
    public mut Func<int, int> Bonus;                     // 기본값: 아무것도 하지 않고 기본값(0)을 돌려줌
    public mut List<Action> Todo;
    public void Heal(int n) { Hp += n; }
    public void Update(float dt)
    {
        int k = 3;
        Func<int, int> addK = x => x + k;                // 불변 지역 변수 k의 복사본을 캡처
        Bonus = x => x + Hp;                             // this를 캡처: Hp는 참조를 통해 읽음
        Todo.Add(() => { Hp -= 1; });                    // 참조를 통한 쓰기: 지연 명령
        foreach (var t in Todo) { t(); }
    }
}

Apply(Rules.Add, 3, 4); Apply((a, b) => a * b, 3, 4);   // 정적 메서드(메서드 그룹)도 값
Action<int> repair = ship.Heal;                          // 참조에 묶인 엔티티 메서드
var add5 = Adder(5); add5(1); add5.Invoke(1);
```

- **타입**: `Func<A1, ..., R>`(마지막 인자가 반환형), `Action`, `Action<A1, ...>`, `delegate R Name(A1 a, ...);` 선언. `Func`/`Action`은 매개변수와 반환형이 같으면 같은 타입이고, 이름 있는 델리게이트는 각각 다른 타입입니다(C#과 같이 서로 바꿔 담을 수 없음). 매개변수는 값 매개변수만(`ref`/`out`/`own`/`mut`/범위는 E0268), 반환형은 컬렉션(E0268: 어느 함수가 만든 것인지 모르는 컬렉션을 받는 쪽이 소유하게 됨)이나 살아 있는 엔티티 참조(E0268: `Ship?`로)일 수 없습니다. 델리게이트는 **null이 아닙니다**(`Func<int>?`는 E0210): 기본값(`default`, 초기값 없는 필드, `new Action[4]`의 원소)은 아무것도 하지 않고 반환형의 기본값을 돌려줍니다. C#의 null 델리게이트를 부르면 `NullReferenceException`이 나는 것과 다릅니다.
- **값**: 델리게이트 타입을 기대하는 곳(타입이 있는 변수·필드·매개변수·반환·원소·`Spawn`/`new` 초기화)의 람다(식 본문이나 블록 본문, 블록은 모든 경로에서 반환해야 함: E0207), 정적 메서드(`Rules.Add`, 같은 클래스 안에서는 `Add`), 참조에 묶인 엔티티 메서드(`ship.Heal`, 엔티티 메서드 안에서는 `Heal` = `this.Heal`). 메서드는 매개변수와 반환형이 델리게이트와 정확히 같아야 하고(오버로드 중에서 그런 것을 고름, 없으면 E0268), struct 인스턴스 메서드·코루틴·`extern` 함수는 람다로 감쌉니다(E0268). `var f = x => x * 2;`처럼 타입이 없으면 E0027입니다. 제네릭 메서드는 다른 인자에서 `T`를 추론한 뒤 람다에 그 타입을 줍니다(`Ap(x => x + 1, 2)`에서 `T = int`).
- **호출**: `f(x)`, `f.Invoke(x)`, 델리게이트 필드 `F(x)`/`ship.F(x)`, 원소 `fs[0](x)`, 호출 결과 `Adder(5)(1)`. 컴파일러는 델리게이트 타입마다 그 타입의 값에 들어갈 수 있는 함수를 **모두 압니다**(닫힌 세계: 프로그램의 모든 람다와 메서드 그룹). 그래서 호출은 함수 번호에 따른 **정적 분기**로 컴파일되고(함수 포인터 없음), 효과 분석은 그 함수들의 효과를 합칩니다: 델리게이트를 부르는 시스템의 스케줄·해저드·병렬화·샤드 실행 방식이 그 함수들이 하는 일을 반영합니다(`sijoc explain`의 `[delegates]`가 타입마다 함수와 캡처를 보여 줌).
- **캡처는 값 복사**입니다. 람다가 쓰는 바깥의 지역 변수·매개변수는 델리게이트를 만들 때 복사됩니다. Sijo의 바인딩은 기본이 불변이라 C#의 변수 캡처와 결과가 같고, 그 차이가 생길 수 있는 `mut` 지역 변수·`ref`/`out` 매개변수와 컬렉션은 캡처할 수 없습니다(E0268이 불변 지역 변수에 복사하라고 알려 줌). 엔티티 메서드 안의 람다가 필드나 메서드를 쓰면 `this`(엔티티 참조)를 캡처하고, 본문은 그 엔티티에 **참조를 통해** 닿습니다: 읽기는 시스템 시작 시점 값, 쓰기·변경하는 메서드 호출은 지연 명령입니다. 같은 엔티티의 시스템 안에서 불러도 마찬가지입니다(`var me = this; me.Hp -= 1;`과 같은 규칙, 0장의 시스템 규칙). `base` 호출(E0268)과 `mut` struct 메서드의 `this`(E0268)는 캡처할 수 없습니다.
- **엔티티가 사라지면**: 살아 있는 엔티티 참조(`this`, `Ship s`, 쿼리 커서)를 캡처한 델리게이트를 그 엔티티가 파괴된 뒤에 부르면 **아무 일도 하지 않고 기본값을 돌려줍니다**. 파괴된 엔티티에 보낸 메시지가 버려지는 것과 같은 규칙이라 null 검사가 필요 없습니다. nullable 참조(`Ship?`)로 캡처한 것은 본문이 직접 검사합니다.
- **델리게이트를 담는 델리게이트**: 같은 타입의 델리게이트를 캡처할 수 있습니다(`Compose`, 호출 사슬 `Chain = x => prev(x) + t`). 고정 크기 값 안에 자기 타입을 담을 수 없으므로 그런 캡처는 문자열과 같은 메모리(프레임 아레나 → 틱 경계 수집기, 3.8절)의 상자에 들어가고, 필드에 남으면 수집기가 승격·표시합니다. 해제 코드는 필요 없습니다.
- **담을 수 있는 곳**: 지역 변수, 매개변수, 반환 값, struct 필드, 엔티티 필드(저장 상태·라이브 코딩·체크섬·샤드 사본 포함), 리스트·배열·사전 값. 컴포넌트 필드(E0264), 원격 메시지·RPC의 인자(E0248/E0608: 함수는 서버에서만 돔), 클라이언트 코드(만들기·부르기·복제되는 필드: E0268), 사전 키, `==`·`Print`·보간(E0239)에는 쓸 수 없습니다. `atomic`의 `fail`은 델리게이트 필드도 되돌립니다.
- **저장 상태**: 델리게이트 값은 함수의 소스 위치 이름(`Ship.Update__lambda2`: 그 메서드 안 몇 번째 람다)과 캡처한 값의 타입, 캡처한 값으로 저장됩니다. 라이브 코딩이나 새 빌드에서 그 함수가 없어졌거나 캡처가 바뀌었으면 기본값으로 불러옵니다(필드 이주 규칙과 같음, 10.3절).
- 생성 코드: 델리게이트 타입마다 C struct `{ int32_t fn; union { 함수마다 캡처 struct } env; }`, 람다마다 캡처를 앞 매개변수로 받는 정적 함수, 호출은 `switch (fn)`. 함수 포인터가 없으므로 C 컴파일러가 인라인할 수 있고 결과는 스레드·샤드 수와 무관하게 같습니다.

### 1.14.2 격자: `Grid<T>`, `Grid3<T>`

```csharp
public mut Grid<ECell> Cells = new Grid<ECell>(K.W, K.H);       // = ECell[,] (Grid3<T>는 T[,,])
Cells.Step((x, y, c) => y == 0 && Random.Int(0, 10) == 0 ? ECell.Sand : c);       // 맵: 자기 칸만 읽음
Cells.Move(ECell.Empty, (x, y, c) => c == ECell.Sand && Cells.Get(x, y + 1, ECell.Wall) == ECell.Empty
    ? Grid.Dir(0, 1) : Grid.Stay);                                                   // 이동
Heat.Step((x, y, h) => (Heat.Get(x - 1, y, 0) + Heat.Get(x + 1, y, 0) + 2 * h) / 4);  // 스텐실
int hit = Cells.Ray(x0, y0, x1, y1, c => c != ECell.Empty);  // 선분 위 첫 칸까지의 걸음 수, 없으면 -1
int n = Cells.Flood(x, y, ECell.Water);                      // 시작 칸과 같은 연결 영역을 채운 칸 수
```

`Grid<T>`는 2차원 배열 `T[,]`, `Grid3<T>`는 3차원 배열 `T[,,]`의 다른 이름이고 아래 메서드는 2·3차원 배열 모두에 있습니다. 그래서 복제(원소 단위 델타), 저장·복원, 체크섬, 상수 크기 필드는 다차원 배열 그대로입니다. 칸은 `g[x, y]`(범위 밖은 패닉), `Width`·`Height`·`Depth`, `InBounds(x, y)`, `Get(x, y, fallback)`(범위 밖이면 fallback), `Fill(v)`.

| 연산 | 의미 | 실행 |
|---|---|---|
| `Step((x, y, c) => 새 값)` | 모든 칸의 새 값을 **연산 전의 격자**로 계산(람다 안의 `g[...]`·`Get`은 이전 값) | 자기 칸만 읽으면 제자리(맵), 상수 오프셋 이웃을 읽으면 두 번째 버퍼(스텐실)에 쓰고 가장자리에서 그 오프셋보다 먼 칸은 경계 검사 없이 계산. 칸들을 스레드로 나눠 병렬 |
| `Move(empty, (x, y, c) => Grid.Dir(dx, dy))` | 각 칸이 이웃(각 축 -1..1) 하나로 가겠다고 제안(`Grid.Stay`는 그대로). `empty`인 칸은 자기에게 오겠다는 칸 중 **정해진 순서에서 첫 칸**을 받고 그 칸은 `empty`가 됨. 순서: 직선 이동(+y, -y, +x, -x, +z, -z) 먼저, 그다음 대각선(+y 쪽, 그다음 +x 쪽 먼저). 진 칸은 이번 틱에 그대로 | 각 칸의 결과가 이전 격자만으로 정해지므로(이웃의 제안은 필요한 곳에서 다시 계산) 병렬이고 결정론적 |
| `Ray(x0, y0, x1, y1, c => 맞음)` | 선분의 칸(축 거리 중 큰 값 n에 대해 n+1칸, 가장 가까운 칸으로 반올림)을 따라 처음 맞는 칸의 걸음 수, 격자를 벗어나거나 끝나면 -1 | 직렬 |
| `Flood(x, y, v)` | 시작 칸과 비트 단위로 같은 칸들 중 면으로 이어진 영역을 `v`로(너비 우선), 바뀐 칸 수 | 직렬 |

- 람다는 칸마다 값을 계산할 뿐 아무것도 바꿀 수 없습니다: 대입, 리스트 변경, `Print`/`Draw`, `Spawn`, 효과가 있는 메서드·`[Pure]`가 아닌 extern 호출은 E0707입니다(칸은 어떤 순서로든, 여러 스레드에서 계산되므로). 본문은 식이나 `{ return 식; }`이고, 경우가 많으면 `?:`, switch 식, 정적 도우미 함수를 씁니다.
- 람다 안의 `Random`은 **칸으로 정해지는 수**(연산마다 엔티티의 난수 흐름에서 한 번 뽑은 키, 칸 번호, 칸 안에서 몇 번째인지)라서 스레드 수와 무관하게 같습니다. 람다가 부르는 함수 안의 `Random`은 엔티티의 순서 있는 흐름을 쓰므로 E0707입니다.
- `Move`·`Flood`의 칸 타입은 숫자·bool·char·enum, `Step`은 struct도 됩니다. `Grid.Dir`의 각 성분은 -1, 0, 1입니다(상수가 아니면 실행 중에 검사).
- `sijoc explain`의 `[grids]`가 연산마다 맵/스텐실/이동 판정, 오프셋 반경, 검사 없이 읽는 곳의 수를 보여 줍니다. 칸이 4096개 이상이고 이미 병렬 영역 안이 아니면 병렬로 돕니다.

## 1.15 인터페이스

```csharp
public interface IDamageable
{
    void Damage(int amount);
    int Health();
    bool IsAlive() => Health() > 0;                    // 기본 구현: 선언하지 않은 클래스·struct가 씀
    int Threat => Health() * 2;                        // 기본 구현이 있는 get 전용 속성
    void Finish() { Damage(Health()); }
}
public interface IArmored : IDamageable { int Armor(); int Threat => Health() * 2 + Armor(); }   // 더 구체적인 기본 구현
public interface IHealable : IDamageable { void Heal(int amount); }

public abstract class Unit : Entity, IDamageable      // 기반 클래스가 먼저, 그다음 인터페이스
{
    public mut int Hp = 10;
    public IDamageable? Target;                         // 구현한 어느 엔티티든 가리킬 수 있음
    public void Damage(int amount) { Hp -= amount; }
    public abstract int Health();                       // 추상 클래스는 서브클래스에 맡길 수 있음
    public bool IsAlive() => Hp > -5;                   // 클래스의 구현이 기본 구현보다 우선
}

public interface IContainer<T> { void Put(T x); T Peek(); int Count(); bool IsEmpty() => Count() == 0; }
public class Crate : Entity, IContainer<int> { ... }   // 제네릭 인터페이스: 타입 인자마다 인스턴스
public struct SStack<T> : IContainer<T> { ... }        // 제네릭 struct도 구현

foreach (var d in Query<IDamageable>()) { if (d is IHealable h) { h.Heal(2); } }
foreach (var c in Query<IContainer<int>>()) { c.Put(1); }
public static float TotalArea<T>(List<T> xs) where T : IShape { ... }   // struct도 구현 가능
public static T Top<T>(IContainer<T> c) => c.Peek();                    // Top(crate)에서 T = int
```

- 인터페이스는 **메서드 시그니처와 기본 구현**, 그리고 **정적 멤버**를 가집니다. 필드·생성자·연산자·제네릭 메서드·async 메서드·`static virtual`은 오류(E0262)입니다. 다른 인터페이스를 확장할 수 있습니다(순환은 E0214).
- **정적 멤버**(C# 8·11):
  - `static abstract T Zero();`: 구현하는 클래스·struct마다 같은 시그니처의 `public static` 메서드를 가져야 하고(없거나 인스턴스 메서드이거나 시그니처가 다르면 E0261), 제네릭 코드에서 타입 매개변수로 부릅니다: `static T Sum<T>(List<T> xs) where T : INum<T> { mut T acc = T.Zero(); ... T.Add(acc, x) ... }`. 인터페이스 이름으로 부르거나(`INum<int>.Zero()`) 기본 구현에서 그냥 부르면 E0262입니다(어느 타입의 것인지 모름). 제네릭은 인스턴스마다 검사·컴파일되므로 이 호출도 그 타입의 정적 메서드로 바로 컴파일됩니다(가상 호출 없음).
  - 본문이 있는 `static float Total(List<SCircle> cs) { ... }`: 인터페이스 자신의 정적 함수입니다(`IShape.Total(cs)`). C#과 같이 구현하는 타입이 물려받지 않습니다(`SCircle.Total`은 E0101).
  - 같은 제네릭 인터페이스의 두 인스턴스를 구현할 때 매개변수가 같은 `static abstract` 멤버는 다른 메서드처럼 반환형까지 같아야 합니다(E0261).
  - 엔티티 핸들이 가리키는 엔티티의 정적 멤버를 부르는 방법은 없습니다(정적 멤버는 아키타입 디스패치의 대상이 아님).
- **기본 구현**(C# 8): 본문이 있는 인터페이스 메서드는 그 메서드를 선언하지 않은 클래스와 struct가 씁니다. 클래스(또는 기반 클래스)가 직접 구현하면 그것이 우선합니다. 기본 구현 안의 `this`는 인터페이스이므로 **그 인터페이스의 메서드만** 부를 수 있고, 구현하는 쪽의 필드와 다른 메서드는 보이지 않습니다(E0101/E0108, 필요한 값을 `int Hp();`처럼 인터페이스에 선언하라는 노트). 엔티티 클래스가 쓰는 기본 구현은 **아키타입마다 따로 컴파일**되어 그 안의 호출(`Damage(Health())`)도 그 아키타입의 구현으로 정적 디스패치되고, 다른 엔티티에서 부르면 시스템 규칙대로 지연 메시지나 스냅샷 읽기가 됩니다. struct는 기본 구현의 사본을 자기 메서드로 받아 **struct마다** 검사·컴파일하고 `where T : I`에서 정적으로 부릅니다(오류는 `in the default implementation 'I.M' that struct 'S' uses` 노트와 함께). struct의 `mut` 메서드를 부르는 기본 구현은 `mut void GrowTwice() { Grow(); Grow(); }`처럼 `mut`으로 선언합니다. 그러면 struct가 받는 사본이 `this`를 바꾸는 `mut` 메서드가 되고(엔티티는 어차피 자기 필드를 바꾸므로 상관없음), 본문 없는 시그니처의 `mut`은 E0028입니다(구현마다 정함). 기본 구현 `Update`는 그것을 쓰는 엔티티의 시스템이고, 기본 구현 `client` 메서드는 클라이언트 시스템입니다.
- 어느 기본 구현을 쓰는지: 메서드를 다시 선언한 **가장 구체적인 인터페이스**의 것입니다. `IArmored : IDamageable`이 `Threat`의 기본 구현을 다시 주면 `IArmored`를 구현한 클래스는 그것을 쓰고, 본문 없이 다시 선언하면 기본 구현이 없어져 클래스가 구현해야 합니다. 다시 선언할 때는 시그니처가 같아야 합니다(E0261). 서로 확장하지 않는 두 인터페이스가 이름과 매개변수가 같은 기본 구현을 가지거나 한쪽에만 기본 구현이 있으면, 그런 메서드는 클래스에 하나뿐이므로 클래스가 직접 구현해야 합니다(E0261이 어느 선언들이 겹치는지 알려 줌). 매개변수가 다르면 서로 다른 오버로드입니다(1.4.2절).
- **속성**: Sijo의 속성은 식 본문 get 전용 속성 `int Hp => ...;`뿐이고 `{ get; }` 접근자는 없으므로(E0016), 인터페이스의 속성은 **기본 구현이 있는 속성**(`int Threat => Health() * 2;`)입니다. 구현하는 쪽이 값을 정해야 하면 메서드 `int Health();`로 선언합니다. 구현은 인터페이스 쪽이 속성이면 속성(`public int Threat => ...;`), 메서드면 메서드여야 합니다(부르는 모양이 달라서, E0261).
- **제네릭 인터페이스** `interface IContainer<T>`는 제네릭 struct처럼 **쓰인 타입 인자마다 따로** 선언·검사됩니다(`IContainer<int>`와 `IContainer<string>`은 다른 인터페이스). `: Entity, IContainer<int>`, 핸들 `IContainer<int>?`, `Query<IContainer<int>>()`, `x is IContainer<int> c`, `where T : IContainer<int>`(`where C : IContainer<T>`처럼 다른 타입 매개변수로도), 확장(`interface IPair<T> : IGet<T>`), 인스턴스마다 검사하는 `where` 제약(E0260), 제네릭 struct의 구현(`struct SStack<T> : IContainer<T>`)이 됩니다. 핸들 인자에서 타입 인자를 추론합니다(`Top(crate)`: `Crate`가 구현한 `IContainer<int>`에서 `T = int`). 기본 구현의 오류는 그 인스턴스에서 `in 'IContainer<string>.IsEmpty', an instance of this generic interface` 노트와 함께 나오고, 한 번도 쓰이지 않은 인스턴스는 검사되지 않습니다.
- 제네릭 인터페이스의 제약: 한 클래스는 같은 제네릭 인터페이스의 여러 인스턴스를 구현할 수 있고, `T`를 받는 메서드는 인스턴스마다 오버로드가 됩니다(`Put(int)`, `Put(float)`, 1.4.2절). 매개변수까지 같고 반환형이나 기본 구현만 다른 메서드(`T Peek()`)가 있으면 두 인스턴스를 함께 구현할 수 없습니다(E0261). 타입 매개변수는 불변이고(`in`/`out`은 E0015), 타입 인자의 수가 틀리거나 빠지면 E0228, 타입 이름과 같은 타입 매개변수는 E0258입니다. .NET의 인터페이스(`IEquatable<T>`, `IComparable<T>`)는 Sijo에 없으므로 알 수 없는 타입(E0105)이고, 필요하면 프로그램에 선언합니다.
- 엔티티 클래스는 `: Entity, I1, I2`처럼 기반 클래스를 먼저 쓰고 인터페이스를 나열합니다. 구체 클래스는 (기반 클래스를 포함해) 기본 구현이 없는 모든 메서드를 같은 시그니처(매개변수 타입과 모드 `ref`/`out`/`own`, 범위, 반환형, `client`, 속성인지)로 가져야 하고(E0261), 추상 클래스는 서브클래스에 맡길 수 있습니다. 필드 이름이 인터페이스 메서드의 이름과 같으면 E0104입니다(이름은 필드이거나 메서드이지 둘 다는 아님).
- `I`/`I?` 타입의 값은 그 인터페이스를 구현한 엔티티의 핸들입니다. 호출은 추상 기반 클래스와 똑같이 **구현한 아키타입마다 단형화된 정적 디스패치**이고(기본 구현을 쓰는 아키타입은 그 아키타입용으로 컴파일된 기본 구현으로), 다른 엔티티에 대한 호출이면 시스템 규칙대로 지연 메시지(쓰기)나 스냅샷 읽기가 됩니다. `Query<I>()`는 구현한 모든 아키타입을 돌고, `x is I y`·핸들 필드 복제·`where T : I`가 됩니다. 인터페이스는 `Spawn`할 수 없습니다(E0246).
- struct도 인터페이스를 구현할 수 있지만(인스턴스 메서드만, `client` 메서드가 있는 인터페이스는 불가) struct 값을 인터페이스 변수에 담을 수는 없습니다(박싱 없음, E0200). 대신 제네릭 메서드의 `where T : I` 제약으로 씁니다. 제네릭 엔티티 클래스도 구현할 수 있고(`class Pool<T> : Entity, IScore`), 인스턴스마다 따로 검사합니다(E0261).
- 린트: 인터페이스 이름은 `I`로 시작합니다(L114, 두 프리셋 모두). 인터페이스 메서드와 그 구현의 매개변수는 쓰이지 않아도 경고하지 않습니다(시그니처가 정해져 있으므로). LSP의 이름 바꾸기·참조 찾기는 인터페이스 메서드, 그 기본 구현과 구현, struct가 쓰는 기본 구현, 제네릭 인터페이스의 모든 인스턴스를 한 메서드로 다룹니다.

## 1.16 컴포넌트: 런타임에 붙였다 떼는 상태

```csharp
public component Burning                         // 엔티티에 붙는 데이터 + 메서드
{
    public mut int Left = 3;                     // 필드 규칙은 엔티티 필드와 같음(mut, 범위 타입, 특성)
    public int in 0..9 Power = 1;                // mut이 없으면 Add에서 정한 값 그대로

    public void Update(float dt)                 // Burning이 붙은 모든 엔티티마다 도는 시스템
    {
        Left -= 1;
        if (Owner is Ship s)                     // Owner: 이 컴포넌트를 가진 엔티티(핸들)
        {
            s.Hp -= Power;                       // 핸들을 통한 쓰기: 지연 +=
        }
        if (Left <= 0)
        {
            Remove<Burning>();                   // 받는 쪽이 없으면 이 엔티티
        }
    }

    public void Fuel(int ticks) { Left += ticks; }
}

public class Ship : Entity
{
    public mut int Hp = 100;

    public void Update(float dt)
    {
        if (Has<Stunned>()) { return; }                      // 시스템 시작 시점의 조합
        foreach (var o in Query<Ship>())
        {
            if (o != this && o.Get<Burning>() is Burning b)  // 없으면 null: is로 검사
            {
                b.Fuel(2);                                   // 다른 엔티티의 컴포넌트: 지연 메시지
            }
        }
        Add(new Burning { Left = 5 });                       // 이미 있으면 값을 바꿈
    }
}

foreach (var (s, b) in Query<Ship, Burning>()) { Print($"{s.Hp} {b.Left}"); }   // Burning이 붙은 Ship
foreach (var b in Query<Burning>()) { ... }              // 어느 클래스든 Burning이 붙은 엔티티
int n = Count<Burning>();
```

- **선언**: `component C { ... }`는 필드, 메서드(`Update(float dt)`는 시스템, `client` 메서드는 클라이언트 시스템, `async` 메서드는 코루틴 시스템), 상수, 정적 메서드를 가집니다. 기반 클래스·인터페이스(`: X`), 생성자(E0217), `virtual`·`override`·`abstract`·제네릭 메서드, `static`/`abstract` 컴포넌트는 E0264입니다. 필드는 메시지 인자가 담을 수 있는 것을 담습니다: 숫자·bool·`char`·enum·struct·nullable 엔티티 핸들·**문자열·리스트·배열**(그것들의 리스트 포함). 사전과 델리게이트는 E0264입니다(엔티티 쪽에 둡니다). `component`는 문맥 키워드라 다른 곳에서는 이름으로 쓸 수 있습니다.
- **문자열·컬렉션 필드**: `Add(new C { Hits = xs })`는 메시지 인자처럼 값의 **복사본**을 명령에 담습니다(그 뒤 `xs`를 바꿔도 컴포넌트의 리스트는 그대로, 64KB까지). 붙은 컴포넌트의 리스트는 그 엔티티의 것이라 컴포넌트 메서드 안에서 `Hits.Add(...)`로 바꾸고(필드는 `mut`), 다른 엔티티의 것은 메서드를 불러 바꿉니다(E0405). `Remove<C>()`·파괴·같은 컴포넌트를 다시 붙여 값이 바뀔 때 해제되고, 행이 다른 조합 테이블로 옮겨 갈 때는 소유권이 따라갑니다. 복제·저장 상태·샤드 사본·라이브 코딩 이주는 엔티티 필드와 같습니다.
- **컴포넌트 코루틴**: 컴포넌트의 `async void Run()`은 그 컴포넌트가 있는 엔티티마다 도는 코루틴 시스템이고, 프레임(재개 지점·타이머·`await`를 넘는 지역 변수)은 **그 컴포넌트의 숨은 필드**입니다. 그래서 `Remove<C>()`는 코루틴을 끝내고, 다시 붙이면 처음부터 시작합니다. `await` 규칙은 엔티티 코루틴과 같습니다(1.10절).
- **생성과 함께 붙이기**: `Spawn(new Ship { Hp = 3 }, new Burning { Left = 5 }, new Shield { })`는 그 조합의 테이블에 엔티티를 바로 만듭니다. 컴포넌트의 시스템은 첫 틱부터 그 엔티티를 돌고, `Has`/`Get`도 처음부터 참입니다. 뒤의 인자는 컴포넌트여야 하고(E0265), 같은 컴포넌트를 두 번 줄 수 없습니다(E0104).
- **붙이기·떼기**: `e.Add(new C { F = x })`는 필드 기본값 위에 초기화 목록을 얹은 컴포넌트를 붙이고, `e.Remove<C>()`는 뗍니다. 둘 다 `Spawn`/`Destroy`처럼 **시스템 끝에 명령 순서대로** 적용됩니다(4.9절). 같은 컴포넌트를 다시 붙이면 값이 바뀌고, 없는 것을 떼면 아무 일도 없습니다. `e`는 살아 있음이 보장된 핸들(`Ship`, `if (x is Ship s)` 뒤의 `s`, 쿼리 커서)이고, 엔티티·컴포넌트 메서드 안에서는 받는 쪽을 빼면 `this`입니다(정적 코드에서 받는 쪽 없이 쓰면 E0265). 컴포넌트가 아닌 것을 붙이거나 떼면(`Add(new SVec2())`, `Remove<Ship>()`) E0265, 컴포넌트를 `new`나 `Spawn`으로 혼자 만들면 E0265입니다.
- **검사·조회**: `e.Has<C>()`는 `bool`, `e.Get<C>()`는 `C?`(같은 엔티티를 그 컴포넌트로 본 핸들, 없으면 null)입니다. 둘 다 **시스템 시작 시점의 조합**을 보므로 `Add` 바로 뒤의 `Has`는 아직 `false`입니다. `Get`은 nullable이라 `if (e.Get<C>() is C c)`로 검사한 뒤에 씁니다(검사 없이 `e.Get<C>().F`는 E0301이 그 검사를 알려 줌). `e is C`는 쓰지 않습니다(E0235가 `Has`/`Get`을 알려 줌).
- **다른 엔티티의 컴포넌트**는 핸들로 닿으므로 시스템 규칙 그대로입니다: 읽기는 시작 시점 값, 쓰기(`c.Left += 2`)와 메서드 호출(`c.Fuel(5)`)은 지연 명령, `mut`이 없는 필드에 쓰면 E0310, 쓴 뒤 같은 시스템에서 다시 읽으면 W0201.
- **컴포넌트 안에서** `this`와 필드 이름은 그 컴포넌트입니다(자기 접근). 엔티티는 `Owner`(아무 엔티티나 담는 `Entity` 타입의 핸들)로 닿습니다: `Owner is Ship s`, `Owner.Has<Stunned>()`, `Destroy(Owner)`. `Owner`를 통한 쓰기는 다른 핸들처럼 지연됩니다. 컴포넌트 안의 `Destroy()`/`Destroy(this)`는 무엇을 뜻하는지 모호하므로 E0265가 `Remove<C>()`(컴포넌트 떼기)와 `Destroy(Owner)`(엔티티 파괴)를 알려 줍니다. C#(Unity)의 `Destroy(this)`는 컴포넌트를 떼는 것이라 헷갈리기 쉽기 때문입니다.
- **쿼리**: `Query<T, C1, C2>()`는 T(클래스·인터페이스·컴포넌트) 중 C1, C2가 모두 붙은 엔티티를 돕니다. `foreach (var s in ...)`의 `s`는 T 커서이고, C#의 분해처럼 `foreach (var (s, c1, c2) in ...)`로 쓰면 같은 행을 타입마다 본 커서를 받습니다(타입 인자 수와 이름 수가 다르면 E0231). `Count<C>()`는 C가 붙은 엔티티 수입니다.
- **저장할 수 없는 것**: 컴포넌트는 엔티티의 일부이고 언제든 떨어질 수 있으므로 필드·struct 멤버·매개변수·반환 값·컬렉션 원소의 타입이 될 수 없습니다(E0263). 엔티티 핸들(`Ship?`)을 담고 필요할 때 `Get`합니다. 한 시스템 안의 지역 변수(`Burning? b = s.Get<Burning>();`)는 됩니다.
- **클라이언트 코드**는 `Has`/`Get`과 컴포넌트 필드를 읽고(읽는 필드만 복제, 5.9절), 직접 붙이거나 떼지는 못합니다(E0601). 붙이는 엔티티 메서드를 부르면 그 호출이 자동 RPC가 됩니다.
- **조합의 수**: 한 클래스가 가질 수 있는 컴포넌트 조합은 컴파일 타임에 세며(4.4.1절, `Spawn`이 주는 조합에서도 출발) 16개까지는 조합마다 테이블을 둡니다. 더 많으면 그 클래스는 열린 테이블 하나(컴포넌트마다 한 비트)를 쓰고, 한 클래스에 붙을 수 있는 컴포넌트 종류가 32개를 넘으면 E0266입니다. 어느 쪽이든 의미는 같습니다(열린 테이블의 엔티티는 행 순서로 방문됨).
