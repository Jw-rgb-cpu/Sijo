# bench/shards.py — raw results

4-vCPU VM (Intel Xeon), gcc 13 -O2, sijoc 0.1.0, localhost UDP, 300 ticks, minimum of 3 rounds.

| workload | boids (world) | configuration | ms per tick | speed-up vs 1 thread | shard exchange per tick (all shards) | messages per tick |
|---|---|---|---|---|---|---|
| hash | 16000 (800x800) | 1 process, 1 thread | 5.92 | 1.00x | - | - |
| hash | 16000 (800x800) | 1 process, 4 threads | 2.53 | 2.34x | - | - |
| hash | 16000 (800x800) | 2 shard processes x 1 thread | 4.75 | 1.25x | 163.2 KB | 4.0 |
| hash | 16000 (800x800) | 2 shard processes x 2 threads | 3.85 | 1.54x | 163.2 KB | 4.0 |
| hash | 16000 (800x800) | 4 shard processes x 1 thread | 2.80 | 2.11x | 266.7 KB | 24.0 |
| hash | 32000 (1131x1131) | 1 process, 1 thread | 12.72 | 1.00x | - | - |
| hash | 32000 (1131x1131) | 1 process, 4 threads | 5.23 | 2.43x | - | - |
| hash | 32000 (1131x1131) | 2 shard processes x 1 thread | 9.65 | 1.32x | 324.4 KB | 4.0 |
| hash | 32000 (1131x1131) | 2 shard processes x 2 threads | 7.40 | 1.72x | 324.4 KB | 4.0 |
| hash | 32000 (1131x1131) | 4 shard processes x 1 thread | 6.08 | 2.09x | 517.8 KB | 24.0 |
| halo | 16000 (800x800) | 1 process, 1 thread | 6.08 | 1.00x | - | - |
| halo | 16000 (800x800) | 1 process, 4 threads | 2.63 | 2.31x | - | - |
| halo | 16000 (800x800) | 2 shard processes x 1 thread | 4.12 | 1.47x | 10.5 KB | 2.0 |
| halo | 16000 (800x800) | 2 shard processes x 2 threads | 3.20 | 1.90x | 10.5 KB | 2.0 |
| halo | 16000 (800x800) | 4 shard processes x 1 thread | 2.33 | 2.61x | 35.4 KB | 12.0 |
| halo | 32000 (1131x1131) | 1 process, 1 thread | 12.79 | 1.00x | - | - |
| halo | 32000 (1131x1131) | 1 process, 4 threads | 5.24 | 2.44x | - | - |
| halo | 32000 (1131x1131) | 2 shard processes x 1 thread | 8.48 | 1.51x | 17.7 KB | 2.0 |
| halo | 32000 (1131x1131) | 2 shard processes x 2 threads | 5.93 | 2.16x | 17.7 KB | 2.0 |
| halo | 32000 (1131x1131) | 4 shard processes x 1 thread | 4.53 | 2.82x | 52.6 KB | 12.0 |
