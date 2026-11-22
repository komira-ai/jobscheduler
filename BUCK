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
