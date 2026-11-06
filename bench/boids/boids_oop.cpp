// boids_oop.cpp — idiomatic object-oriented C++ for the "boids" workload.
//
// Each boid is a heap-allocated Actor with a virtual Tick(); neighbours are
// found through a registry of Boid* (std::vector<Boid*>), and each boid reads
// the neighbour objects through those pointers.
//
// Jacobi double-buffering, done explicitly as an OOP programmer would: Boid
// overrides PreTick() to copy Pos/Vel into PrevPos/PrevVel; the world calls
// PreTick() on every boid before the boid tick group (common/oop.hpp), and a
// boid reads only other->PrevPos / other->PrevVel. Its own Pos/Vel are read
// and written directly. Same algorithm and float operation order as boids.sijo.
#include <cstdio>

#include "../common/sijo_compat.h"
#include "../common/oop.hpp"

namespace {

constexpr float kWorldSize = 200.0f;
constexpr float kViewSq = 144.0f;
constexpr float kMaxSpeed = 30.0f;
constexpr int kGroupBoids = 0;

int32_t Q(float v) { return nb_f2i(v * 8388608.0f); }

Vec2 Limit(Vec2 v, float max) {
    float l2 = v.LengthSq();
    if (l2 > max * max) {
        return v * (max / std::sqrt(l2));
    }
    return v;
}

// [Range(0, 200)] on Pos in the Sijo source: clamped on every write
Vec2 ClampToWorld(Vec2 p) { return Vec2(nb_clamp_range(p.X, 0.0f, kWorldSize), nb_clamp_range(p.Y, 0.0f, kWorldSize)); }

class FlockWorld;

class Boid : public Actor {
public:
    Boid(Vec2 pos, Vec2 vel) : Actor(kGroupBoids, /*needsPreTick=*/true), Pos(ClampToWorld(pos)), Vel(vel) {}

    void BeginPlay() override;
    void EndPlay() override;
    void PreTick() override {
        PrevPos = Pos;
        PrevVel = Vel;
    }
    void Tick(float dt) override;

    Vec2 Pos;
    Vec2 Vel;
    int32_t Neighbours = 0;
    Vec2 PrevPos;  // start-of-tick snapshot, read by other boids
    Vec2 PrevVel;

private:
    FlockWorld* Flock() const;
};

class FlockWorld : public World {
public:
    std::vector<Boid*> boids;

    static std::unique_ptr<Boid> Make(int i) {
        int32_t s1 = nb_next((int32_t)((uint32_t)i * 7919u + 17u));
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        int32_t s4 = nb_next(s3);
        return std::make_unique<Boid>(Vec2(nb_unit(s1) * kWorldSize, nb_unit(s2) * kWorldSize),
                                      Vec2(nb_unit(s3) * 20.0f - 10.0f, nb_unit(s4) * 20.0f - 10.0f));
    }

    // scatter: same spawn (= iteration) order, but the objects are allocated in a shuffled
    // order, so neighbours in the actor list are far apart in memory (an "aged" heap).
    void Start(int n, bool scatter) {
        if (scatter) {
            std::vector<std::unique_ptr<Boid>> objs((size_t)n);
            int32_t* perm = bench_permutation(n);
            for (int k = 0; k < n; k++) objs[(size_t)perm[k]] = Make(perm[k]);
            std::free(perm);
            for (int i = 0; i < n; i++) SpawnActor(std::move(objs[(size_t)i]));
        } else {
            for (int i = 0; i < n; i++) SpawnActor(Make(i));
        }
        FlushDeferred();
    }

    void Tick(int tick, float dt, bool quiet) {
        RunTickGroup(kGroupBoids, dt);
        if (tick % 100 == 0) {
            int32_t crowded = 0, h = 0;
            for (const Boid* b : boids) {
                if (b->Neighbours > 6) crowded++;
                int32_t e = Q(b->Pos.X);
                e = nb_mix31(e, Q(b->Pos.Y));
                e = nb_mix31(e, Q(b->Vel.X));
                e = nb_mix31(e, Q(b->Vel.Y));
                e = nb_mix31(e, b->Neighbours);
                h = nb_hash_add(h, e);
            }
            if (!quiet) std::printf("bench tick=%d n=%d crowded=%d h=%d\n", tick, (int)boids.size(), crowded, h);
        }
    }

    uint64_t WorldChecksum() const {
        uint64_t h = nc_begin();
        int32_t row = 0;
        for (const Boid* b : boids) {
            h = nc_handle(h, row++);
            h = nc_f(h, b->Pos.X);
            h = nc_f(h, b->Pos.Y);
            h = nc_f(h, b->Vel.X);
            h = nc_f(h, b->Vel.Y);
            h = nc_i(h, b->Neighbours);
        }
        return h;
    }
};

FlockWorld* Boid::Flock() const { return static_cast<FlockWorld*>(GetWorld()); }
void Boid::BeginPlay() { Flock()->boids.push_back(this); }
void Boid::EndPlay() { EraseFromRegistry(Flock()->boids, this); }

void Boid::Tick(float dt) {
    Vec2 center(0.0f, 0.0f);
    Vec2 align(0.0f, 0.0f);
    Vec2 away(0.0f, 0.0f);
    int32_t count = 0;
    for (const Boid* other : Flock()->boids) {
        if (other == this) {
            continue;
        }
        Vec2 offset = other->PrevPos - Pos;
        float distSq = offset.LengthSq();
        if (distSq < kViewSq) {
            center = center + other->PrevPos;
            align = align + other->PrevVel;
            if (distSq < 9.0f) {
                away = away - offset;
            }
            count++;
        }
    }
    Neighbours = count;
    if (count > 0) {
        float inv = 1.0f / (float)count;
        Vec2 toCenter = center * inv - Pos;
        Vec2 avgVel = align * inv - Vel;
        Vel = Vel + toCenter * 0.6f * dt + avgVel * 1.5f * dt + away * 4.0f * dt;
    }
    Vel = Limit(Vel, kMaxSpeed);
    Pos = ClampToWorld(Pos + Vel * dt);
    if (Pos.X <= 0.0f || Pos.X >= kWorldSize) {
        Vel = Vec2(-Vel.X, Vel.Y);
    }
    if (Pos.Y <= 0.0f || Pos.Y >= kWorldSize) {
        Vel = Vec2(Vel.X, -Vel.Y);
    }
}

}  // namespace

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 1000);
    const float dt = 1.0f / 60.0f;
    double t0 = bench_now();
    double t1;
    auto world = std::make_unique<FlockWorld>();
    world->Start(a.n, a.scatter != 0);
    for (int t = 1; t <= a.ticks; t++) world->Tick(t, dt, a.quiet != 0);
    t1 = bench_now();
    std::printf("threads: 1\n");
    std::printf("world checksum: %016llx\n", (unsigned long long)world->WorldChecksum());
    bench_report_time(&a, t0, t1);
    world.release();  // skip teardown (not timed, and the other implementations do not free either)
    return 0;
}
