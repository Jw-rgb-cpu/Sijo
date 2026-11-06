// Framework.cs — the C# twin of bench/common/oop.hpp: a minimal Unity/Unreal-style
// actor framework (virtual Tick, tick groups, deferred Spawn/Destroy, weak references,
// typed registries maintained in BeginPlay/EndPlay). See oop.hpp for the rationale.
using System;
using System.Collections.Generic;
using System.Diagnostics;

namespace SijoBench;

public struct Vec2
{
    public float X;
    public float Y;

    public Vec2(float x, float y)
    {
        X = x;
        Y = y;
    }

    public readonly float LengthSq() => X * X + Y * Y;
    public readonly float Length() => MathF.Sqrt(X * X + Y * Y);
    public static Vec2 operator +(Vec2 a, Vec2 b) => new Vec2(a.X + b.X, a.Y + b.Y);
    public static Vec2 operator -(Vec2 a, Vec2 b) => new Vec2(a.X - b.X, a.Y - b.Y);
    public static Vec2 operator *(Vec2 a, float s) => new Vec2(a.X * s, a.Y * s);
}

public abstract class Actor
{
    internal World? WorldRef;
    internal int ObjectIndex = -1;
    internal uint Serial;
    internal bool PendingKill;
    internal readonly int TickGroup;
    internal readonly bool WantsPreTick;

    protected Actor(int group, bool needsPreTick = false)
    {
        TickGroup = group;
        WantsPreTick = needsPreTick;
    }

    public World World => WorldRef!;
    public virtual void BeginPlay() { }
    public virtual void EndPlay() { }
    public virtual void PreTick() { }
    public abstract void Tick(float dt);
    // deferred: the actor is removed at the end of the current tick group
    public void Destroy()
    {
        if (!PendingKill)
        {
            PendingKill = true;
            World.KillList.Add(this);
        }
    }
}

public class World
{
    public const int MaxGroups = 4;
    public long Spawned;
    public long Destroyed;

    internal readonly List<Actor> KillList = new();
    private readonly List<Actor> actors = new();
    private readonly List<Actor> pendingSpawns = new();
    private readonly List<Actor>[] ticking = { new(), new(), new(), new() };
    private readonly List<Actor>[] preTick = { new(), new(), new(), new() };
    private Actor?[] objects = new Actor?[1024];   // like GUObjectArray
    private uint[] serials = new uint[1024];
    private int objectCount;
    private readonly Stack<int> freeSlots = new();

    public T SpawnActor<T>(T actor) where T : Actor
    {
        actor.WorldRef = this;
        int index;
        if (freeSlots.Count > 0)
        {
            index = freeSlots.Pop();
        }
        else
        {
            if (objectCount == objects.Length)
            {
                Array.Resize(ref objects, objects.Length * 2);
                Array.Resize(ref serials, serials.Length * 2);
            }
            index = objectCount++;
        }
        actor.ObjectIndex = index;
        objects[index] = actor;
        serials[index] += 1;
        actor.Serial = serials[index];
        pendingSpawns.Add(actor);
        return actor;
    }

    public void RunTickGroup(int group, float dt)
    {
        foreach (Actor a in preTick[group])
        {
            a.PreTick();
        }
        foreach (Actor a in ticking[group])
        {
            a.Tick(dt);
        }
        FlushDeferred();
    }

    public void FlushDeferred()
    {
        if (KillList.Count > 0)
        {
            foreach (Actor a in KillList)
            {
                a.EndPlay();
                objects[a.ObjectIndex] = null;
                serials[a.ObjectIndex] += 1;
                freeSlots.Push(a.ObjectIndex);
                Destroyed++;
            }
            KillList.Clear();
            for (int g = 0; g < MaxGroups; g++)
            {
                preTick[g].RemoveAll(static a => a.PendingKill);
                ticking[g].RemoveAll(static a => a.PendingKill);
            }
            actors.RemoveAll(static a => a.PendingKill);
        }
        if (pendingSpawns.Count > 0)
        {
            foreach (Actor a in pendingSpawns)
            {
                ticking[a.TickGroup].Add(a);
                if (a.WantsPreTick)
                {
                    preTick[a.TickGroup].Add(a);
                }
                actors.Add(a);
                a.BeginPlay();
                Spawned++;
            }
            pendingSpawns.Clear();
        }
    }

    internal Actor? Lookup(int index, uint serial) => serials[index] == serial ? objects[index] : null;
}

// Unreal-style weak object pointer (index + serial number): never dangles.
public readonly struct WeakRef<T> where T : Actor
{
    private readonly int index;
    private readonly uint serial;

    public WeakRef(T? actor)
    {
        index = actor?.ObjectIndex ?? -1;
        serial = actor?.Serial ?? 0;
    }

    public T? Get(World w) => index < 0 ? null : (T?)w.Lookup(index, serial);
}

public static class Num
{
    public static int Next(int s) => unchecked(s * 1664525 + 1013904223);

    public static float Unit(int s) => (float)((s >> 8) & 65535) / 65536.0f;

    // Sijo's float -> int cast (NaN -> 0, saturating)
    public static int F2I(float f)
    {
        if (float.IsNaN(f))
        {
            return 0;
        }
        if (f >= 2147483647.0f)
        {
            return int.MaxValue;
        }
        if (f <= -2147483648.0f)
        {
            return int.MinValue;
        }
        return (int)f;
    }

    public static float ClampRange(float x, float lo, float hi) => float.IsNaN(x) ? lo : (x < lo ? lo : (x > hi ? hi : x));

    public static int Mix31(int e, int v) => unchecked(e * 31 + v);

    public static int HashAdd(int h, int e) => unchecked(h + (e ^ 1540483477) * 668265261);

    // Sijo runtime world checksum (FNV-1a over 4-byte fields), see common/sijo_compat.h
    public static ulong FnvBegin() => 0xcbf29ce484222325UL;

    public static ulong Fnv(ulong h, int v)
    {
        uint u = unchecked((uint)v);
        for (int i = 0; i < 4; i++)
        {
            h ^= (u >> (8 * i)) & 0xff;
            h = unchecked(h * 0x100000001b3UL);
        }
        return h;
    }

    public static ulong Fnv(ulong h, float v) => Fnv(h, BitConverter.SingleToInt32Bits(v));

    public static ulong FnvHandle(ulong h, int row) => Fnv(Fnv(h, row + 1), 1);
}

public sealed class Args
{
    public int N;
    public int Ticks = 600;
    public bool Quiet;
    public bool Timing;
    public float Arena;

    public static Args Parse(string[] argv, int start, int defaultN)
    {
        var a = new Args { N = defaultN };
        for (int i = start; i < argv.Length; i++)
        {
            switch (argv[i])
            {
                case "--n": a.N = int.Parse(argv[++i]); break;
                case "--ticks": a.Ticks = int.Parse(argv[++i]); break;
                case "--arena": a.Arena = float.Parse(argv[++i], System.Globalization.CultureInfo.InvariantCulture); break;
                case "--quiet": a.Quiet = true; break;
                case "--timing": a.Timing = true; break;
                default: throw new ArgumentException("unknown option " + argv[i]);
            }
        }
        if (a.Ticks < 1)
        {
            a.Ticks = 1;
        }
        return a;
    }

    public void ReportTime(Stopwatch sw)
    {
        if (Timing)
        {
            double ms = sw.Elapsed.TotalMilliseconds;
            Console.WriteLine($"time: {ms:F3} ms total, {ms * 1000.0 / (Ticks + 1):F2} us/tick");
        }
    }
}
