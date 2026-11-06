// shooter_oop.cpp — idiomatic object-oriented C++ for the "shooter" workload.
//
// Ship and Bullet are heap-allocated Actors with virtual Tick(); ships tick in
// tick group 0, bullets in tick group 1 (the order of the Sijo systems).
// References between actors are Unreal-style weak pointers (WeakPtr<Ship>), so
// bullets whose owner died never touch freed memory. Spawn/Destroy are
// deferred to the end of the tick group (common/oop.hpp).
//
// Semantics vs. shooter.sijo:
//  * Jacobi reads: ships read other ships' positions in Steer() and when aiming.
//    Ship overrides PreTick() to snapshot Pos into PrevPos before the ship tick
//    group, and other ships read only PrevPos.
//  * Cross-entity writes (ship->Hp -= Damage, owner->Kills += 1) are done
//    immediately, the OOP way. That is equivalent to Sijo's deferred commutative
//    writes because nothing in the bullet tick group reads Hp or Kills.
// Single-threaded, like typical gameplay code.
#include <cstdio>

#include "../common/oop.hpp"

namespace {

constexpr int kMaxHp = 100;
constexpr float kHitRadiusSq = 1.5f * 1.5f;
constexpr float kBulletSpeed = 20.0f;
constexpr float kReload = 0.6f;
constexpr int kDamage = 11;  // Rules.DamageFor(2)
constexpr int kGroupShips = 0;
constexpr int kGroupBullets = 1;

enum class ETeam : uint8_t { Red, Blue };

int32_t Q(float v) { return nb_f2i(v * 1048576.0f); }

class ArenaWorld;

class Ship : public Actor {
public:
    Ship(int32_t id, ETeam side, float arena) : Actor(kGroupShips, /*needsPreTick=*/true), Side(side), Id(id) {
        int32_t s1 = nb_next((int32_t)((uint32_t)id * 7919u + 17u));
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        int32_t s4 = nb_next(s3);
        Pos = Vec2(nb_unit(s1) * arena, nb_unit(s2) * arena);
        Vel = Vec2(nb_unit(s3) * 6.0f - 3.0f, nb_unit(s4) * 6.0f - 3.0f);
        Cooldown = nb_unit(s4) * kReload;
    }

    void BeginPlay() override;
    void EndPlay() override;
    void PreTick() override { PrevPos = Pos; }
    void Tick(float dt) override;

    Vec2 Pos;
    Vec2 Vel;
    int32_t Hp = kMaxHp;
    ETeam Side;
    float Cooldown = 0.0f;
    int32_t Kills = 0;
    int32_t Id;
    WeakPtr<Ship> Target;
    Vec2 PrevPos;  // start-of-tick snapshot, read by other ships

private:
    ArenaWorld* Arena() const;
    void Steer();
};

class Bullet : public Actor {
public:
    Bullet(Vec2 pos, Vec2 vel, const Ship* owner, int32_t damage) : Actor(kGroupBullets), Pos(pos), Vel(vel), Owner(owner), Damage(damage) {}

    void BeginPlay() override;
    void EndPlay() override;
    void Tick(float dt) override;

    Vec2 Pos;
    Vec2 Vel;
    WeakPtr<Ship> Owner;
    int32_t Damage = 5;
    float Life = 1.5f;

private:
    ArenaWorld* Arena() const;
};

class ArenaWorld : public World {
public:
    explicit ArenaWorld(int ships, float arena) : NumShips(ships), Size(arena) {}

    std::vector<Ship*> ships;      // registries, maintained by BeginPlay/EndPlay
    std::vector<Bullet*> bullets;
    const int NumShips;
    const float Size;

    Vec2 Wrap(Vec2 pos) const {
        float x = pos.X;
        float y = pos.Y;
        if (x < 0.0f) x += Size;
        if (x >= Size) x -= Size;
        if (y < 0.0f) y += Size;
        if (y >= Size) y -= Size;
        return Vec2(x, y);
    }

    void Start() {
        for (int i = 0; i < NumShips; i++) {
            SpawnActor(std::make_unique<Ship>(i, i % 2 == 0 ? ETeam::Red : ETeam::Blue, Size));
        }
        FlushDeferred();
    }

    void Tick(int tick, float dt, bool quiet) {
        RunTickGroup(kGroupShips, dt);
        RunTickGroup(kGroupBullets, dt);
        if (tick % 100 == 0) {
            int32_t hs = 0, hb = 0, kills = 0;
            for (const Ship* s : ships) {
                int32_t e = s->Id;
                e = nb_mix31(e, s->Hp);
                e = nb_mix31(e, s->Kills);
                e = nb_mix31(e, Q(s->Pos.X));
                e = nb_mix31(e, Q(s->Pos.Y));
                e = nb_mix31(e, Q(s->Cooldown));
                hs = nb_hash_add(hs, e);
                kills = (int32_t)((uint32_t)kills + (uint32_t)s->Kills);
            }
            for (const Bullet* b : bullets) {
                int32_t e = Q(b->Pos.X);
                e = nb_mix31(e, Q(b->Pos.Y));
                e = nb_mix31(e, Q(b->Vel.X));
                e = nb_mix31(e, Q(b->Vel.Y));
                e = nb_mix31(e, Q(b->Life));
                hb = nb_hash_add(hb, e);
            }
            if (!quiet) {
                std::printf("bench tick=%d ships=%d bullets=%d kills=%d hs=%d hb=%d\n", tick, (int)ships.size(), (int)bullets.size(), kills,
                            hs, hb);
            }
        }
    }
};

ArenaWorld* Ship::Arena() const { return static_cast<ArenaWorld*>(GetWorld()); }
ArenaWorld* Bullet::Arena() const { return static_cast<ArenaWorld*>(GetWorld()); }
void Ship::BeginPlay() { Arena()->ships.push_back(this); }
void Ship::EndPlay() { EraseFromRegistry(Arena()->ships, this); }
void Bullet::BeginPlay() { Arena()->bullets.push_back(this); }
void Bullet::EndPlay() { EraseFromRegistry(Arena()->bullets, this); }

void Ship::Steer() {
    const Ship* best = nullptr;
    float bestD = 1000000000.0f;
    int32_t bestId = 0;
    for (const Ship* other : Arena()->ships) {
        if (other->Side != Side) {
            float distSq = (other->PrevPos - Pos).LengthSq();
            if (distSq < bestD || (distSq == bestD && other->Id < bestId)) {
                bestD = distSq;
                bestId = other->Id;
                best = other;
            }
        }
    }
    Target = best;
}

void Ship::Tick(float dt) {
    Steer();
    ArenaWorld* arena = Arena();
    Pos = arena->Wrap(Pos + Vel * dt);
    Cooldown -= dt;
    if (Cooldown <= 0.0f) {
        if (Ship* target = Target.Get(arena)) {
            Vec2 dir = target->PrevPos - Pos;
            float len = dir.Length();
            if (len > 0.001f) {
                arena->SpawnActor(std::make_unique<Bullet>(Pos, dir * (kBulletSpeed / len), this, kDamage));
            }
            Cooldown = kReload;
        }
    }
    if (Hp <= 0) {
        // replaced by a fresh ship of the same team
        arena->SpawnActor(std::make_unique<Ship>(Id + arena->NumShips, Side, arena->Size));
        Destroy();
    }
}

void Bullet::Tick(float dt) {
    ArenaWorld* arena = Arena();
    Pos = Pos + Vel * dt;
    Life -= dt;
    if (Life <= 0.0f || Pos.X < 0.0f || Pos.Y < 0.0f || Pos.X > arena->Size || Pos.Y > arena->Size) {
        Destroy();
        return;
    }
    Ship* owner = Owner.Get(arena);
    bool hit = false;
    for (Ship* ship : arena->ships) {
        if (ship != owner && (ship->Pos - Pos).LengthSq() < kHitRadiusSq) {
            ship->Hp -= Damage;
            if (owner != nullptr) {
                owner->Kills += 1;
            }
            hit = true;
        }
    }
    if (hit) {
        Destroy();
    }
}

}  // namespace

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 500);
    if (a.arena <= 0.0f) a.arena = 256.0f;
    const float dt = 1.0f / 60.0f;
    double t0 = bench_now();
    auto world = std::make_unique<ArenaWorld>(a.n, a.arena);
    world->Start();
    for (int t = 1; t <= a.ticks; t++) world->Tick(t, dt, a.quiet != 0);
    double t1 = bench_now();
    std::printf("threads: 1\n");
    std::printf("entities alive: Ship=%d Bullet=%d (spawned %lld, destroyed %lld)\n", (int)world->ships.size(), (int)world->bullets.size(),
                (long long)world->spawned, (long long)world->destroyed);
    bench_report_time(&a, t0, t1);
    world.release();  // skip teardown (not timed, and the other implementations do not free either)
    return 0;
}
