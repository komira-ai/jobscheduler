#!/usr/bin/env python3
"""Reference for the order js-measure runs a plan in (src/plan.rs), written from the
published SplitMix64 and the Fisher-Yates shuffle, not from the Rust code.

Run it: it checks the generator against the published test vector (seed 1234567, as in
the rand_xoshiro crate's `reference` test and Vigna's splitmix64.c) and against the orders
pinned in tests/suite/plan.rs, and prints them. A change to the Rust draw or shuffle that
moves a pinned order must move this file in the same change.
"""

MASK = (1 << 64) - 1


class SplitMix64:
    def __init__(self, seed):
        self.state = seed

    def next(self):
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)


def schedule(settings, arms, runs, warmups, seed):
    """(setting, arm, warmup) in execution order: warm-ups first, each group shuffled.
    A cell is a setting and an arm, numbered setting-major, repeated `count` times."""
    rng = SplitMix64(seed)
    out = []
    for count, warmup in ((warmups, True), (runs, False)):
        cells = [c for c in range(settings * arms) for _ in range(count)]
        for at in range(len(cells) - 1, 0, -1):
            swap = rng.next() % (at + 1)
            cells[at], cells[swap] = cells[swap], cells[at]
        out += [(c // arms, c % arms, warmup) for c in cells]
    return out


if __name__ == "__main__":
    rng = SplitMix64(1234567)
    vector = [rng.next() for _ in range(5)]
    assert vector == [
        6457827717110365317,
        3203168211198807973,
        9817491932198370423,
        4593380528125082431,
        16408922859458223821,
    ], vector
    one = schedule(1, 3, 2, 1, 42)
    assert [a for _, a, _ in one] == [0, 2, 1, 1, 0, 2, 1, 2, 0]
    two = schedule(1, 3, 2, 0, 7)
    assert [a for _, a, _ in two] == [0, 2, 0, 1, 2, 1]
    cells = schedule(2, 2, 2, 0, 7)
    assert [(s, a) for s, a, _ in cells] == [
        (0, 0), (1, 0), (1, 0), (0, 1), (1, 1), (0, 0), (0, 1), (1, 1),
    ]
    print("ok")
