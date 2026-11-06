// Workloads.cs — idiomatic object-oriented C# (Unity MonoBehaviour style) for the three
// benchmark workloads. Same algorithms, float operation order and Jacobi emulation
// (PreTick snapshots) as the OOP C++ versions in bench/*/ *_oop.cpp.
using System;
using System.Collections.Generic;
using System.Diagnostics;

namespace SijoBench;

// ------------------------------------------------------------------ particles

public sealed class Particle : Actor
{
    private const float Gravity = 9.81f;
    private const float Bounce = 0.8f;
    private const float Friction = 0.95f;

    public Vec2 Pos;
    public Vec2 Vel;
    public float Life;
    public int Seed;

    public Particle(Vec2 pos, Vec2 vel, float life, int seed) : base(0)
    {
        Pos = pos;
        Vel = vel;
        Life = life;
        Seed = seed;
    }

    public override void BeginPlay() => ((ParticleWorld)World).Particles.Add(this);
    public override void EndPlay() => ((ParticleWorld)World).Particles.Remove(this);

    public override void Tick(float dt)
    {
        Vel = new Vec2(Vel.X, Vel.Y - Gravity * dt);
        Pos = Pos + Vel * dt;
        if (Pos.Y < 0.0f)
        {
            Pos = new Vec2(Pos.X, -Pos.Y);
            Vel = new Vec2(Vel.X * Friction, -Vel.Y * Bounce);
        }
        Life -= dt;
        if (Life <= 0.0f)
        {
            int s1 = Num.Next(Seed);
            int s2 = Num.Next(s1);
            int s3 = Num.Next(s2);
            Seed = s3;
            Pos = new Vec2(0.0f, 1.0f);
            Vel = new Vec2(Num.Unit(s1) * 20.0f - 10.0f, Num.Unit(s2) * 15.0f + 5.0f);
            Life = 1.0f + Num.Unit(s3) * 3.0f;
        }
    }
}

public sealed class ParticleWorld : World
{
    public readonly List<Particle> Particles = new();

    private static int Q(float v) => Num.F2I(v * 8388608.0f);

    public static void Run(Args a)
    {
        const float dt = 1.0f / 60.0f;
        var sw = Stopwatch.StartNew();
        var w = new ParticleWorld();
        for (int i = 0; i < a.N; i++)
        {
            int s1 = Num.Next(unchecked(i * 7919 + 17));
            int s2 = Num.Next(s1);
            int s3 = Num.Next(s2);
            int s4 = Num.Next(s3);
            w.SpawnActor(new Particle(new Vec2(Num.Unit(s1) * 40.0f - 20.0f, Num.Unit(s2) * 10.0f),
                new Vec2(Num.Unit(s3) * 20.0f - 10.0f, Num.Unit(s4) * 15.0f), Num.Unit(s4) * 4.0f, s4));
        }
        w.FlushDeferred();
        for (int t = 1; t <= a.Ticks; t++)
        {
            w.RunTickGroup(0, dt);
            if (t % 100 == 0)
            {
                int h = 0;
                foreach (Particle p in w.Particles)
                {
                    int e = Q(p.Pos.X);
                    e = Num.Mix31(e, Q(p.Pos.Y));
                    e = Num.Mix31(e, Q(p.Vel.X));
                    e = Num.Mix31(e, Q(p.Vel.Y));
                    e = Num.Mix31(e, Q(p.Life));
                    e = Num.Mix31(e, p.Seed);
                    h = Num.HashAdd(h, e);
                }
                if (!a.Quiet)
                {
                    Console.WriteLine($"bench tick={t} n={w.Particles.Count} h={h}");
                }
            }
        }
        sw.Stop();
        ulong c = Num.FnvBegin();
        int row = 0;
        foreach (Particle p in w.Particles)
        {
            c = Num.FnvHandle(c, row++);
            c = Num.Fnv(c, p.Pos.X);
            c = Num.Fnv(c, p.Pos.Y);
            c = Num.Fnv(c, p.Vel.X);
            c = Num.Fnv(c, p.Vel.Y);
            c = Num.Fnv(c, p.Life);
            c = Num.Fnv(c, p.Seed);
        }
        Console.WriteLine("threads: 1");
        Console.WriteLine($"world checksum: {c:x16}");
        a.ReportTime(sw);
    }
}

// ------------------------------------------------------------------ boids

public sealed class Boid : Actor
{
    private const float WorldSize = 200.0f;
    private const float ViewSq = 144.0f;
    private const float MaxSpeed = 30.0f;

    public Vec2 Pos;
    public Vec2 Vel;
    public int Neighbours;
    public Vec2 PrevPos;   // start-of-tick snapshot, read by other boids
    public Vec2 PrevVel;

    public Boid(Vec2 pos, Vec2 vel) : base(0, needsPreTick: true)
    {
        Pos = ClampToWorld(pos);
        Vel = vel;
    }

    private static Vec2 ClampToWorld(Vec2 p) => new Vec2(Num.ClampRange(p.X, 0.0f, WorldSize), Num.ClampRange(p.Y, 0.0f, WorldSize));

    private static Vec2 Limit(Vec2 v, float max)
    {
        float l2 = v.LengthSq();
        if (l2 > max * max)
        {
            return v * (max / MathF.Sqrt(l2));
        }
        return v;
    }

    public override void BeginPlay() => ((BoidWorld)World).Boids.Add(this);
    public override void EndPlay() => ((BoidWorld)World).Boids.Remove(this);

    public override void PreTick()
    {
        PrevPos = Pos;
        PrevVel = Vel;
    }

    public override void Tick(float dt)
    {
        var center = new Vec2(0.0f, 0.0f);
        var align = new Vec2(0.0f, 0.0f);
        var away = new Vec2(0.0f, 0.0f);
        int count = 0;
        foreach (Boid other in ((BoidWorld)World).Boids)
        {
            if (other == this)
            {
                continue;
            }
            Vec2 offset = other.PrevPos - Pos;
            float distSq = offset.LengthSq();
            if (distSq < ViewSq)
            {
                center = center + other.PrevPos;
                align = align + other.PrevVel;
                if (distSq < 9.0f)
                {
                    away = away - offset;
                }
                count++;
            }
        }
        Neighbours = count;
        if (count > 0)
        {
            float inv = 1.0f / count;
            Vec2 toCenter = center * inv - Pos;
            Vec2 avgVel = align * inv - Vel;
            Vel = Vel + toCenter * 0.6f * dt + avgVel * 1.5f * dt + away * 4.0f * dt;
        }
        Vel = Limit(Vel, MaxSpeed);
        Pos = ClampToWorld(Pos + Vel * dt);
        if (Pos.X <= 0.0f || Pos.X >= WorldSize)
        {
            Vel = new Vec2(-Vel.X, Vel.Y);
        }
        if (Pos.Y <= 0.0f || Pos.Y >= WorldSize)
        {
            Vel = new Vec2(Vel.X, -Vel.Y);
        }
    }
}

public sealed class BoidWorld : World
{
    public readonly List<Boid> Boids = new();

    private static int Q(float v) => Num.F2I(v * 8388608.0f);

    public static void Run(Args a)
    {
        const float dt = 1.0f / 60.0f;
        var sw = Stopwatch.StartNew();
        var w = new BoidWorld();
        for (int i = 0; i < a.N; i++)
        {
            int s1 = Num.Next(unchecked(i * 7919 + 17));
            int s2 = Num.Next(s1);
            int s3 = Num.Next(s2);
            int s4 = Num.Next(s3);
            w.SpawnActor(new Boid(new Vec2(Num.Unit(s1) * 200.0f, Num.Unit(s2) * 200.0f),
                new Vec2(Num.Unit(s3) * 20.0f - 10.0f, Num.Unit(s4) * 20.0f - 10.0f)));
        }
        w.FlushDeferred();
        for (int t = 1; t <= a.Ticks; t++)
        {
            w.RunTickGroup(0, dt);
            if (t % 100 == 0)
            {
                int crowded = 0, h = 0;
                foreach (Boid b in w.Boids)
                {
                    if (b.Neighbours > 6)
                    {
                        crowded++;
                    }
                    int e = Q(b.Pos.X);
                    e = Num.Mix31(e, Q(b.Pos.Y));
                    e = Num.Mix31(e, Q(b.Vel.X));
                    e = Num.Mix31(e, Q(b.Vel.Y));
                    e = Num.Mix31(e, b.Neighbours);
                    h = Num.HashAdd(h, e);
                }
                if (!a.Quiet)
                {
                    Console.WriteLine($"bench tick={t} n={w.Boids.Count} crowded={crowded} h={h}");
                }
            }
        }
        sw.Stop();
        ulong c = Num.FnvBegin();
        int row = 0;
        foreach (Boid b in w.Boids)
        {
            c = Num.FnvHandle(c, row++);
            c = Num.Fnv(c, b.Pos.X);
            c = Num.Fnv(c, b.Pos.Y);
            c = Num.Fnv(c, b.Vel.X);
            c = Num.Fnv(c, b.Vel.Y);
            c = Num.Fnv(c, b.Neighbours);
        }
        Console.WriteLine("threads: 1");
        Console.WriteLine($"world checksum: {c:x16}");
        a.ReportTime(sw);
    }
}

// ------------------------------------------------------------------ shooter

public enum ETeam : byte
{
    Red,
    Blue
}

public sealed class Ship : Actor
{
    public const int MaxHp = 100;
    public const float Reload = 0.6f;
    public const float BulletSpeed = 20.0f;
    public const int Damage = 11;

    public Vec2 Pos;
    public Vec2 Vel;
    public int Hp = MaxHp;
    public ETeam Side;
    public float Cooldown;
    public int Kills;
    public int Id;
    public WeakRef<Ship> Target;
    public Vec2 PrevPos;   // start-of-tick snapshot, read by other ships

    public Ship(int id, ETeam side, float arena) : base(0, needsPreTick: true)
    {
        int s1 = Num.Next(unchecked(id * 7919 + 17));
        int s2 = Num.Next(s1);
        int s3 = Num.Next(s2);
        int s4 = Num.Next(s3);
        Pos = new Vec2(Num.Unit(s1) * arena, Num.Unit(s2) * arena);
        Vel = new Vec2(Num.Unit(s3) * 6.0f - 3.0f, Num.Unit(s4) * 6.0f - 3.0f);
        Cooldown = Num.Unit(s4) * Reload;
        Side = side;
        Id = id;
    }

    private ArenaWorld Arena => (ArenaWorld)World;
    public override void BeginPlay() => Arena.Ships.Add(this);
    public override void EndPlay() => Arena.Ships.Remove(this);
    public override void PreTick() => PrevPos = Pos;

    private void Steer()
    {
        Ship? best = null;
        float bestD = 1000000000.0f;
        int bestId = 0;
        foreach (Ship other in Arena.Ships)
        {
            if (other.Side != Side)
            {
                float distSq = (other.PrevPos - Pos).LengthSq();
                if (distSq < bestD || (distSq == bestD && other.Id < bestId))
                {
                    bestD = distSq;
                    bestId = other.Id;
                    best = other;
                }
            }
        }
        Target = new WeakRef<Ship>(best);
    }

    public override void Tick(float dt)
    {
        Steer();
        ArenaWorld arena = Arena;
        Pos = arena.Wrap(Pos + Vel * dt);
        Cooldown -= dt;
        if (Cooldown <= 0.0f)
        {
            Ship? target = Target.Get(arena);
            if (target != null)
            {
                Vec2 dir = target.PrevPos - Pos;
                float len = dir.Length();
                if (len > 0.001f)
                {
                    arena.SpawnActor(new Bullet(Pos, dir * (BulletSpeed / len), this, Damage));
                }
                Cooldown = Reload;
            }
        }
        if (Hp <= 0)
        {
            arena.SpawnActor(new Ship(Id + arena.NumShips, Side, arena.Size));
            Destroy();
        }
    }
}

public sealed class Bullet : Actor
{
    private const float HitRadiusSq = 1.5f * 1.5f;

    public Vec2 Pos;
    public Vec2 Vel;
    public WeakRef<Ship> Owner;
    public int Damage = 5;
    public float Life = 1.5f;

    public Bullet(Vec2 pos, Vec2 vel, Ship owner, int damage) : base(1)
    {
        Pos = pos;
        Vel = vel;
        Owner = new WeakRef<Ship>(owner);
        Damage = damage;
    }

    private ArenaWorld Arena => (ArenaWorld)World;
    public override void BeginPlay() => Arena.Bullets.Add(this);
    public override void EndPlay() => Arena.Bullets.Remove(this);

    public override void Tick(float dt)
    {
        ArenaWorld arena = Arena;
        Pos = Pos + Vel * dt;
        Life -= dt;
        if (Life <= 0.0f || Pos.X < 0.0f || Pos.Y < 0.0f || Pos.X > arena.Size || Pos.Y > arena.Size)
        {
            Destroy();
            return;
        }
        Ship? owner = Owner.Get(arena);
        bool hit = false;
        foreach (Ship ship in arena.Ships)
        {
            if (ship != owner && (ship.Pos - Pos).LengthSq() < HitRadiusSq)
            {
                ship.Hp -= Damage;
                if (owner != null)
                {
                    owner.Kills += 1;
                }
                hit = true;
            }
        }
        if (hit)
        {
            Destroy();
        }
    }
}

public sealed class ArenaWorld : World
{
    public readonly List<Ship> Ships = new();
    public readonly List<Bullet> Bullets = new();
    public readonly int NumShips;
    public readonly float Size;

    public ArenaWorld(int ships, float size)
    {
        NumShips = ships;
        Size = size;
    }

    public Vec2 Wrap(Vec2 pos)
    {
        float x = pos.X;
        float y = pos.Y;
        if (x < 0.0f)
        {
            x += Size;
        }
        if (x >= Size)
        {
            x -= Size;
        }
        if (y < 0.0f)
        {
            y += Size;
        }
        if (y >= Size)
        {
            y -= Size;
        }
        return new Vec2(x, y);
    }

    private static int Q(float v) => Num.F2I(v * 1048576.0f);

    public static void Run(Args a)
    {
        const float dt = 1.0f / 60.0f;
        float size = a.Arena > 0 ? a.Arena : 256.0f;
        var sw = Stopwatch.StartNew();
        var w = new ArenaWorld(a.N, size);
        for (int i = 0; i < a.N; i++)
        {
            w.SpawnActor(new Ship(i, i % 2 == 0 ? ETeam.Red : ETeam.Blue, size));
        }
        w.FlushDeferred();
        for (int t = 1; t <= a.Ticks; t++)
        {
            w.RunTickGroup(0, dt);
            w.RunTickGroup(1, dt);
            if (t % 100 == 0)
            {
                int hs = 0, hb = 0, kills = 0;
                foreach (Ship s in w.Ships)
                {
                    int e = s.Id;
                    e = Num.Mix31(e, s.Hp);
                    e = Num.Mix31(e, s.Kills);
                    e = Num.Mix31(e, Q(s.Pos.X));
                    e = Num.Mix31(e, Q(s.Pos.Y));
                    e = Num.Mix31(e, Q(s.Cooldown));
                    hs = Num.HashAdd(hs, e);
                    kills = unchecked(kills + s.Kills);
                }
                foreach (Bullet b in w.Bullets)
                {
                    int e = Q(b.Pos.X);
                    e = Num.Mix31(e, Q(b.Pos.Y));
                    e = Num.Mix31(e, Q(b.Vel.X));
                    e = Num.Mix31(e, Q(b.Vel.Y));
                    e = Num.Mix31(e, Q(b.Life));
                    hb = Num.HashAdd(hb, e);
                }
                if (!a.Quiet)
                {
                    Console.WriteLine($"bench tick={t} ships={w.Ships.Count} bullets={w.Bullets.Count} kills={kills} hs={hs} hb={hb}");
                }
            }
        }
        sw.Stop();
        Console.WriteLine("threads: 1");
        Console.WriteLine($"entities alive: Ship={w.Ships.Count} Bullet={w.Bullets.Count} (spawned {w.Spawned}, destroyed {w.Destroyed})");
        a.ReportTime(sw);
    }
}

public static class Program
{
    public static int Main(string[] argv)
    {
        if (argv.Length < 1)
        {
            Console.Error.WriteLine("usage: Bench <boids|particles|shooter> [--n N] [--ticks T] [--arena A] [--quiet] [--timing]");
            return 2;
        }
        switch (argv[0])
        {
            case "particles": ParticleWorld.Run(Args.Parse(argv, 1, 100000)); break;
            case "boids": BoidWorld.Run(Args.Parse(argv, 1, 1000)); break;
            case "shooter": ArenaWorld.Run(Args.Parse(argv, 1, 500)); break;
            default: Console.Error.WriteLine("unknown workload " + argv[0]); return 2;
        }
        return 0;
    }
}
