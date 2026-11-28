# Written once by `buildrules sync`; yours after that. The workspace's targets: :check, and one aggregate per step.
load("//:buildrules.bzl", "cargo_workspace")

cargo_workspace(path = ".")

# Read by other members (their extra_srcs); exported by reference, so each stays at its path.
export_file(
    name = "coverage-baseline",
    src = "coverage-baseline",
    mode = "reference",
    visibility = ["PUBLIC"],
)
export_file(
    name = "docs/http-gateway.md",
    src = "docs/http-gateway.md",
    mode = "reference",
    visibility = ["PUBLIC"],
)

# The black-box tests of a release: the published bytes (the .debs, the macOS package, the server
# image) started and driven from outside. release.yml's job `blackbox`, the gate before the farm,
# builds this target on the build farm after `publish` (buildrules runs a test as a build action,
# so the build is the run), and only when it passed is the farm told about the release
# (docs/release.md, step 8a). Each black-box test target joins `test_deps`. None has yet: today
# the gate's tests are the release jobs `check-apt` and `beta-macos-first-install`, which
# `blackbox` needs, and this suite builds nothing.
test_suite(
    name = "blackbox",
    test_deps = [],
)
