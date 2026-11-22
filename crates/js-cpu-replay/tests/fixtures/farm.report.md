# CPU packing replay

14140 jobs over 5.9 h on 6 hosts of 88 cores: 622 core-hours of CPU, a mean of 104.7 cores (19.8% of the farm). Recorded wait p50 1232 s, p90 1861 s, p99 3070 s. The most CPU a run of 10 s or more averaged: 23.3 cores (a shorter run is taken to average at most 24.0).

## Jobs by class

352 classes, 14140 runs that ended with 0. The 15 with the most CPU:

| class | runs | CPU % | wall p50 s | mean p50 | peak p50 | peak/mean p50 | near peak p50, blip to burst |
|---|---|---|---|---|---|---|---|
| 51 | 1061 | 8.1 | 35 | 5.0 | - | - | - |
| 27 | 963 | 7.2 | 30 | 5.3 | - | - | - |
| 77 | 46 | 7.1 | 484 | 2.7 | - | - | - |
| 6 | 462 | 5.4 | 36 | 3.1 | - | - | - |
| 57 | 408 | 4.4 | 51 | 4.4 | - | - | - |
| 29 | 595 | 4.2 | 32 | 5.3 | - | - | - |
| 154 | 145 | 4.2 | 356 | 2.6 | - | - | - |
| 49 | 575 | 3.6 | 29 | 5.4 | - | - | - |
| 162 | 433 | 3.5 | 33 | 5.2 | - | - | - |
| 37 | 400 | 3.4 | 62 | 3.2 | - | - | - |
| 15 | 209 | 2.9 | 213 | 1.7 | - | - | - |
| 8 | 197 | 2.8 | 129 | 2.4 | - | - | - |
| 16 | 248 | 2.3 | 57 | 1.4 | - | - | - |
| 1 | 171 | 1.9 | 120 | 1.8 | - | - | - |
| 47 | 152 | 1.6 | 40 | 4.6 | - | - | - |

| kind | classes | runs % | CPU % |
|---|---|---|---|
| short (wall p50 under 60 s) | 302 | 83.3 | 63.7 |
| long and bursty (peak/mean p50 2 or more) | 0 | 0.0 | 0.0 |
| long and steady | 0 | 0.0 | 0.0 |
| long, no peak recorded | 50 | 16.7 | 36.3 |

## Checks of the replay

Waits, recorded and replayed with the recorded bookings (seconds), of every job and of the jobs that arrived after the first hour. A replayed job is placed at the first one-second tick after it arrives, so a replayed wait is up to a second longer, and every wait here is rounded up to the second:

| | p50 | p90 | p99 | after 1 h: p50 | p90 | p99 |
|---|---|---|---|---|---|---|
| recorded | 1232 | 1861 | 3070 | 1322 | 2353 | 3081 |
| replayed, blip | 505 | 1088 | 2137 | 575 | 1131 | 2182 |
| replayed, burst | 505 | 1088 | 2137 | 575 | 1131 | 2182 |

The jobs as placed, by the contention they recorded (the time some task of the job waited for a CPU, over its wall time), against the slowdown the replay models for them (their lost time over their own wall time):

| recorded contention | runs | recorded, mean % | modelled blip, mean % | modelled burst, mean % |
|---|---|---|---|---|
| none recorded | 14140 | - | 10.1 | 10.1 |
| under 1% | 0 | 0.0 | 0.0 | 0.0 |
| 1% to 10% | 0 | 0.0 | 0.0 | 0.0 |
| 10% or more | 0 | 0.0 | 0.0 | 0.0 |

## Options, arrivals at 1x

| arm | shape | jobs/h | wait p50 s | p90 s | p99 s | jobs per host | host-time over cores % | asked past cores, mean | slowdown % | CPU used % | booked/used | used above booking % | plan error % | host-time above plan % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| recorded bookings | blip | 2466 | 505 | 1088 | 2137 | 5.5 | 0.5 | 16.5 | 0.2 | 20.5 | 3.76 | 3.7 | 57.3 | 0.2 |
| recorded bookings | burst | 2466 | 505 | 1088 | 2137 | 5.5 | 0.5 | 16.5 | 0.2 | 20.5 | 3.76 | 3.7 | 57.3 | 0.2 |
| A median, 1.1 | blip | 2514 | 1 | 39 | 105 | 6.5 | 8.4 | 45.6 | 15.7 | 20.9 | 0.95 | 23.1 | 8.3 | 9.8 |
| A median, 1.1 | burst | 2514 | 1 | 39 | 105 | 6.5 | 8.4 | 45.6 | 15.7 | 20.9 | 0.95 | 23.1 | 8.3 | 9.8 |
| A median, 1.0 | blip | 2514 | 1 | 52 | 127 | 6.2 | 6.4 | 41.2 | 10.4 | 20.9 | 0.94 | 23.7 | 7.5 | 8.5 |
| A median, 1.0 | burst | 2514 | 1 | 52 | 127 | 6.2 | 6.4 | 41.2 | 10.4 | 20.9 | 0.94 | 23.7 | 7.5 | 8.5 |
| A median, 0.9 | blip | 2514 | 1 | 66 | 147 | 5.9 | 4.0 | 32.5 | 5.5 | 20.9 | 0.91 | 24.2 | 6.0 | 6.6 |
| A median, 0.9 | burst | 2514 | 1 | 66 | 147 | 5.9 | 4.0 | 32.5 | 5.5 | 20.9 | 0.91 | 24.2 | 6.0 | 6.6 |
| B reserve z=2, 1.1 | blip | 2514 | 1 | 42 | 106 | 6.2 | 6.8 | 34.9 | 10.4 | 20.9 | 0.94 | 22.8 | 8.3 | 16.6 |
| B reserve z=2, 1.1 | burst | 2514 | 1 | 42 | 106 | 6.2 | 6.8 | 34.9 | 10.4 | 20.9 | 0.94 | 22.8 | 8.3 | 16.6 |
| C measured, 1.0 | blip | 2514 | 1 | 43 | 117 | 6.6 | 8.6 | 49.4 | 17.8 | 20.9 | 0.98 | 23.9 | 7.9 | 12.5 |
| C measured, 1.0 | burst | 2514 | 1 | 43 | 117 | 6.6 | 8.6 | 49.4 | 17.8 | 20.9 | 0.98 | 23.9 | 7.9 | 12.5 |
| C measured, 1.1 | blip | 2514 | 1 | 16 | 89 | 7.2 | 10.9 | 60.6 | 28.0 | 20.9 | 1.04 | 23.5 | 9.9 | 14.3 |
| C measured, 1.1 | burst | 2514 | 1 | 16 | 89 | 7.2 | 10.9 | 60.6 | 28.0 | 20.9 | 1.04 | 23.5 | 9.9 | 14.3 |
| D curve, 1.0 | blip | 2514 | 1 | 38 | 118 | 5.6 | 0.0 | - | 0.0 | 20.9 | 1.02 | 24.6 | 0.0 | 0.0 |
| D curve, 1.0 | burst | 2514 | 1 | 38 | 118 | 5.6 | 0.0 | - | 0.0 | 20.9 | 1.02 | 24.6 | 0.0 | 0.0 |
| D curve, 1.1 | blip | 2514 | 1 | 35 | 102 | 5.7 | 7.4 | 5.1 | 1.7 | 20.9 | 1.02 | 25.0 | 0.0 | 0.0 |
| D curve, 1.1 | burst | 2514 | 1 | 35 | 102 | 5.7 | 7.4 | 5.1 | 1.7 | 20.9 | 1.02 | 25.0 | 0.0 | 0.0 |

Jobs no host admitted even with nothing on the farm, run alone on an empty host, over every row: 0.

## Options, arrivals at 4x

| arm | shape | jobs/h | wait p50 s | p90 s | p99 s | jobs per host | host-time over cores % | asked past cores, mean | slowdown % | CPU used % | booked/used | used above booking % | plan error % | host-time above plan % |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| recorded bookings | blip | 3138 | 6106 | 10131 | 11098 | 7.0 | 0.5 | 20.9 | 0.2 | 26.1 | 3.76 | 3.7 | 72.8 | 0.3 |
| recorded bookings | burst | 3138 | 6106 | 10131 | 11098 | 7.0 | 0.5 | 20.9 | 0.2 | 26.1 | 3.76 | 3.7 | 72.8 | 0.3 |
| A median, 1.1 | blip | 9192 | 369 | 520 | 570 | 24.5 | 50.8 | 31.0 | 19.7 | 76.6 | 1.06 | 20.1 | 22.3 | 41.1 |
| A median, 1.1 | burst | 9192 | 369 | 520 | 570 | 24.5 | 50.8 | 31.0 | 19.7 | 76.6 | 1.06 | 20.1 | 22.3 | 41.1 |
| A median, 1.0 | blip | 9152 | 526 | 779 | 849 | 21.1 | 15.8 | 19.6 | 3.3 | 76.2 | 0.91 | 24.6 | 14.8 | 17.8 |
| A median, 1.0 | burst | 9152 | 526 | 779 | 849 | 21.1 | 15.8 | 19.6 | 3.3 | 76.2 | 0.91 | 24.6 | 14.8 | 17.8 |
| A median, 0.9 | blip | 8481 | 656 | 1093 | 1228 | 19.3 | 6.1 | 35.5 | 2.3 | 70.6 | 0.90 | 24.8 | 13.4 | 11.7 |
| A median, 0.9 | burst | 8481 | 656 | 1093 | 1228 | 19.3 | 6.1 | 35.5 | 2.3 | 70.6 | 0.90 | 24.8 | 13.4 | 11.7 |
| B reserve z=2, 1.1 | blip | 9189 | 386 | 573 | 629 | 23.4 | 43.5 | 26.8 | 14.4 | 76.5 | 1.01 | 21.9 | 21.5 | 40.1 |
| B reserve z=2, 1.1 | burst | 9189 | 386 | 573 | 629 | 23.4 | 43.5 | 26.8 | 14.4 | 76.5 | 1.01 | 21.9 | 21.5 | 40.1 |
| C measured, 1.0 | blip | 9158 | 507 | 759 | 800 | 21.7 | 19.5 | 26.8 | 6.4 | 76.3 | 0.95 | 25.3 | 14.1 | 22.2 |
| C measured, 1.0 | burst | 9158 | 507 | 759 | 800 | 21.7 | 19.5 | 26.8 | 6.4 | 76.3 | 0.95 | 25.3 | 14.1 | 22.2 |
| C measured, 1.1 | blip | 9192 | 301 | 437 | 510 | 32.2 | 63.0 | 62.9 | 57.6 | 76.6 | 1.43 | 15.7 | 44.5 | 59.2 |
| C measured, 1.1 | burst | 9192 | 301 | 437 | 510 | 32.2 | 63.0 | 62.9 | 57.6 | 76.6 | 1.43 | 15.7 | 44.5 | 59.2 |
| D curve, 1.0 | blip | 9211 | 434 | 606 | 689 | 20.5 | 0.0 | - | 0.0 | 76.7 | 1.08 | 25.2 | 0.0 | 0.0 |
| D curve, 1.0 | burst | 9211 | 434 | 606 | 689 | 20.5 | 0.0 | - | 0.0 | 76.7 | 1.08 | 25.2 | 0.0 | 0.0 |
| D curve, 1.1 | blip | 9217 | 380 | 535 | 624 | 21.6 | 61.4 | 5.9 | 4.9 | 76.8 | 1.08 | 25.6 | 0.0 | 0.0 |
| D curve, 1.1 | burst | 9217 | 380 | 535 | 624 | 21.6 | 61.4 | 5.9 | 4.9 | 76.8 | 1.08 | 25.6 | 0.0 | 0.0 |

Jobs no host admitted even with nothing on the farm, run alone on an empty host, over every row: 0.

