// particles_oop.cpp — idiomatic object-oriented C++ for the "particles" workload.
//
// Each particle is a heap-allocated Actor with a virtual Tick(); the world ticks
// them through a std::vector<Actor*> (see common/oop.hpp). Same algorithm and
// float operation order as particles.sijo. Particles only touch themselves, so no
// snapshot (PreTick) is needed. Single-threaded, like typical gameplay code.
#include <cstdio>

#include "../common/sijo_compat.h"
#include "../common/oop.hpp"

namespace {

constexpr float kGravity = 9.81f;
constexpr float kBounce = 0.8f;
constexpr float kFriction = 0.95f;
constexpr int kGroupParticles = 0;

int32_t Q(float v) { return nb_f2i(v * 8388608.0f); }

class Particle : public Actor {
public:
    Particle(Vec2 pos, Vec2 vel, float life, int32_t seed) : Actor(kGroupParticles), Pos(pos), Vel(vel), Life(life), Seed(seed) {}

    void BeginPlay() override;
    void EndPlay() override;

    void Tick(float dt) override {
        Vel = Vec2(Vel.X, Vel.Y - kGravity * dt);
        Pos = Pos + Vel * dt;
        if (Pos.Y < 0.0f) {
            Pos = Vec2(Pos.X, -Pos.Y);
            Vel = Vec2(Vel.X * kFriction, -Vel.Y * kBounce);
        }
        Life -= dt;
        if (Life <= 0.0f) {
            Respawn();
        }
    }

    Vec2 Pos;
    Vec2 Vel;
    float Life;
    int32_t Seed;

private:
    void Respawn() {
        int32_t s1 = nb_next(Seed);
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        Seed = s3;
        Pos = Vec2(0.0f, 1.0f);
        Vel = Vec2(nb_unit(s1) * 20.0f - 10.0f, nb_unit(s2) * 15.0f + 5.0f);
        Life = 1.0f + nb_unit(s3) * 3.0f;
    }
};

class ParticleWorld : public World {
public:
    std::vector<Particle*> particles;  // registry, maintained by BeginPlay/EndPlay

    static std::unique_ptr<Particle> Make(int i) {
        int32_t s1 = nb_next((int32_t)((uint32_t)i * 7919u + 17u));
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        int32_t s4 = nb_next(s3);
        return std::make_unique<Particle>(Vec2(nb_unit(s1) * 40.0f - 20.0f, nb_unit(s2) * 10.0f),
                                          Vec2(nb_unit(s3) * 20.0f - 10.0f, nb_unit(s4) * 15.0f), nb_unit(s4) * 4.0f, s4);
    }

    // scatter: same spawn (= iteration) order, but the objects are allocated in a shuffled
    // order, so neighbours in the actor list are far apart in memory (an "aged" heap).
    void Start(int n, bool scatter) {
        if (scatter) {
            std::vector<std::unique_ptr<Particle>> objs((size_t)n);
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
        RunTickGroup(kGroupParticles, dt);
        if (tick % 100 == 0) {
            int32_t h = 0;
            for (const Particle* p : particles) {
                int32_t e = Q(p->Pos.X);
                e = nb_mix31(e, Q(p->Pos.Y));
                e = nb_mix31(e, Q(p->Vel.X));
                e = nb_mix31(e, Q(p->Vel.Y));
                e = nb_mix31(e, Q(p->Life));
                e = nb_mix31(e, p->Seed);
                h = nb_hash_add(h, e);
            }
            if (!quiet) std::printf("bench tick=%d n=%d h=%d\n", tick, (int)particles.size(), h);
        }
    }

    uint64_t WorldChecksum() const {
        uint64_t h = nc_begin();
        int32_t row = 0;
        for (const Particle* p : particles) {
            h = nc_handle(h, row++);
            h = nc_f(h, p->Pos.X);
            h = nc_f(h, p->Pos.Y);
            h = nc_f(h, p->Vel.X);
            h = nc_f(h, p->Vel.Y);
            h = nc_f(h, p->Life);
            h = nc_i(h, p->Seed);
        }
        return h;
    }
};

void Particle::BeginPlay() { static_cast<ParticleWorld*>(GetWorld())->particles.push_back(this); }
void Particle::EndPlay() { EraseFromRegistry(static_cast<ParticleWorld*>(GetWorld())->particles, this); }

}  // namespace

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 100000);
    const float dt = 1.0f / 60.0f;
    double t0 = bench_now();
    double t1;
    {
        auto world = std::make_unique<ParticleWorld>();
        world->Start(a.n, a.scatter != 0);
        for (int t = 1; t <= a.ticks; t++) world->Tick(t, dt, a.quiet != 0);
        t1 = bench_now();
        std::printf("threads: 1\n");
        std::printf("world checksum: %016llx\n", (unsigned long long)world->WorldChecksum());
        world.release();  // like the other implementations, do not time/perform teardown
    }
    bench_report_time(&a, t0, t1);
    return 0;
}
