// oop.hpp — a minimal "classic GameObject / Unreal Actor" framework for the OOP C++
// baselines. It is deliberately idiomatic, not a strawman:
//
//  * every entity is an Actor subclass with a virtual Tick(dt), allocated
//    individually (std::unique_ptr) and owned by World::actors (spawn order);
//  * actors tick in tick groups (UE: ETickingGroup, Unity: script execution
//    order), group by group, in the order the Sijo systems run;
//  * cross-actor references are Unreal-style weak object pointers
//    (index + serial number into the world's object table, like FWeakObjectPtr),
//    so a destroyed actor is never dereferenced;
//  * Spawn/Destroy during a tick group are deferred to the end of the group
//    (BeginPlay/EndPlay run there), which is also what Sijo's system rule does;
//  * game code keeps typed registries (std::vector<Ship*>) maintained in
//    BeginPlay/EndPlay — the usual way to "find all ships" without scanning
//    every actor (UE's TActorIterator would scan all actors and IsA-check).
//
// Jacobi semantics (Sijo: other entities are read as of the start of the
// system) are emulated explicitly: an actor whose fields are read by other
// actors of the same tick group overrides PreTick() to copy those fields into
// "Prev" members; the world calls PreTick() on all such actors of the group
// before ticking it, and other actors read only the Prev members.
#pragma once

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <memory>
#include <utility>
#include <vector>

#include "bench.h"

struct Vec2 {
    float X = 0.0f;
    float Y = 0.0f;
    Vec2() = default;
    Vec2(float x, float y) : X(x), Y(y) {}
    float LengthSq() const { return X * X + Y * Y; }
    float Length() const { return std::sqrt(X * X + Y * Y); }
    friend Vec2 operator+(Vec2 a, Vec2 b) { return Vec2(a.X + b.X, a.Y + b.Y); }
    friend Vec2 operator-(Vec2 a, Vec2 b) { return Vec2(a.X - b.X, a.Y - b.Y); }
    friend Vec2 operator*(Vec2 a, float s) { return Vec2(a.X * s, a.Y * s); }
};

class World;

class Actor {
public:
    virtual ~Actor() = default;
    virtual void BeginPlay() {}
    virtual void EndPlay() {}
    virtual void PreTick() {}
    virtual void Tick(float dt) = 0;

    void Destroy();  // deferred: the actor is removed at the end of the current tick group
    bool IsPendingKill() const { return pendingKill; }
    World* GetWorld() const { return world; }

protected:
    explicit Actor(int group, bool needsPreTick = false) : tickGroup(group), wantsPreTick(needsPreTick) {}

private:
    friend class World;
    template <class T>
    friend class WeakPtr;
    World* world = nullptr;
    int32_t objectIndex = -1;
    uint32_t serial = 0;
    int tickGroup;
    bool wantsPreTick;
    bool pendingKill = false;
};

struct ObjectSlot {
    Actor* object = nullptr;
    uint32_t serial = 0;
};

class World {
public:
    static constexpr int kMaxGroups = 4;
    virtual ~World() = default;

    // Deferred spawn: the actor becomes visible at the end of the current tick group.
    template <class T>
    T* SpawnActor(std::unique_ptr<T> actor) {
        T* raw = actor.get();
        raw->world = this;
        if (freeSlots.empty()) {
            raw->objectIndex = (int32_t)objects.size();
            objects.push_back(ObjectSlot{});
        } else {
            raw->objectIndex = freeSlots.back();
            freeSlots.pop_back();
        }
        ObjectSlot& slot = objects[(size_t)raw->objectIndex];
        slot.object = raw;
        slot.serial += 1;
        raw->serial = slot.serial;
        pendingSpawns.push_back(std::move(actor));
        return raw;
    }

    void RunTickGroup(int group, float dt) {
        for (Actor* a : preTick[group]) a->PreTick();
        for (Actor* a : ticking[group]) a->Tick(dt);
        FlushDeferred();
    }

    // Removes destroyed actors (stable erase-remove) and adds spawned ones. Destroyed actors
    // are found through the kill list (like UE's pending-kill handling), so a tick group in
    // which nothing died costs nothing here.
    void FlushDeferred() {
        if (!killList.empty()) {
            for (Actor* a : killList) {
                a->EndPlay();
                ObjectSlot& slot = objects[(size_t)a->objectIndex];
                slot.object = nullptr;
                slot.serial += 1;  // invalidates every WeakPtr to it
                freeSlots.push_back(a->objectIndex);
                ++destroyed;
            }
            killList.clear();
            auto dead = [](const Actor* a) { return a->pendingKill; };
            for (int g = 0; g < kMaxGroups; g++) {
                preTick[g].erase(std::remove_if(preTick[g].begin(), preTick[g].end(), dead), preTick[g].end());
                ticking[g].erase(std::remove_if(ticking[g].begin(), ticking[g].end(), dead), ticking[g].end());
            }
            actors.erase(std::remove_if(actors.begin(), actors.end(), [](const std::unique_ptr<Actor>& a) { return a->pendingKill; }),
                         actors.end());
        }
        if (!pendingSpawns.empty()) {
            // BeginPlay may not spawn (it does not in these workloads), so iterate by index.
            for (size_t i = 0; i < pendingSpawns.size(); i++) {
                Actor* a = pendingSpawns[i].get();
                ticking[a->tickGroup].push_back(a);
                if (a->wantsPreTick) preTick[a->tickGroup].push_back(a);
                actors.push_back(std::move(pendingSpawns[i]));
                a->BeginPlay();
                ++spawned;
            }
            pendingSpawns.clear();
        }
    }

    const ObjectSlot& Slot(int32_t index) const { return objects[(size_t)index]; }

    int64_t spawned = 0;
    int64_t destroyed = 0;

protected:
    friend class Actor;
    std::vector<Actor*> killList;
    std::vector<std::unique_ptr<Actor>> actors;  // owning, spawn order
    std::vector<std::unique_ptr<Actor>> pendingSpawns;
    std::vector<Actor*> ticking[kMaxGroups];
    std::vector<Actor*> preTick[kMaxGroups];
    std::vector<ObjectSlot> objects;  // like GUObjectArray
    std::vector<int32_t> freeSlots;
};

inline void Actor::Destroy() {
    if (!pendingKill) {
        pendingKill = true;
        world->killList.push_back(this);
    }
}

// Unreal-style TWeakObjectPtr: never dangles; Get() returns nullptr once the actor is gone.
template <class T>
class WeakPtr {
public:
    WeakPtr() = default;
    WeakPtr(const T* a) : index(a ? a->objectIndex : -1), serial(a ? a->serial : 0) {}  // NOLINT: implicit like UE
    T* Get(const World* w) const {
        if (index < 0) return nullptr;
        const ObjectSlot& s = w->Slot(index);
        return s.serial == serial ? static_cast<T*>(s.object) : nullptr;
    }

private:
    int32_t index = -1;
    uint32_t serial = 0;
};

template <class T>
inline void EraseFromRegistry(std::vector<T*>& v, T* a) {
    auto it = std::find(v.begin(), v.end(), a);
    if (it != v.end()) v.erase(it);
}
