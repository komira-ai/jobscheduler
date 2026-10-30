# Which registries a host may pull from (Linux, Podman)

By default the agent never pulls: `--container-pull never` means a job whose image is not
already on the host ends as `failed_to_start`, so a job cannot make the host reach a
registry of its choosing. The operator puts every image on the host first.

`--container-pull missing` lifts that: the host's Podman pulls an image during `create` when
it is not in local storage. Two things still hold:

- The run's image must be pinned `@sha256:<64 hex>`; a tag or a transport prefix is refused
  before the agent calls Podman, and Podman checks what it pulls against that digest.
- Nothing in the agent limits **which registry** the name points at. With `missing`, a
  fully qualified name goes straight to its registry. The limit is the host's own
  containers configuration, below.

The pull is done by the host's Podman on the host's network, not on the job's network. The
nft allow-list of [egress.md](egress.md) filters only the job container's bridge, so it
neither permits nor blocks a pull: the host must be able to reach the registry itself.

## The pull scope: `/etc/containers/policy.json`

containers/image (Podman, Skopeo, Buildah) reads a signature policy before it copies an
image. Its default is the only thing that decides what is allowed, and `registries.conf` is
no allow-list (it has per-registry `blocked` and mirror rules, and a fully qualified name
does not need it). To allow only some registries, set the default to `reject` and accept
per scope:

```json
{
  "default": [{ "type": "reject" }],
  "transports": {
    "docker": {
      "registry.example.org/team": [{ "type": "insecureAcceptAnything" }]
    }
  }
}
```

[`deploy/js-agent/policy.json.example`](../deploy/js-agent/policy.json.example) is this
file. A scope under `docker` is `host[:port]` or `host[:port]/namespace[/repo]`; the
longest matching scope wins, so `registry.example.org/team` covers every repository under
`team` and nothing else on that host. `insecureAcceptAnything` here means "no signature is
required", which is what a digest-pinned image needs: the digest in the name is what
pins the content, not a signature. Add one scope per prefix you run, and keep the list
as short as the images you actually use.

What this changes on the host:

- A pull from any other registry fails, whoever asks: every user of containers/image on the
  host obeys this file unless they have their own (`~/.config/containers/policy.json` for a
  rootless user). Put it only on a host that runs nothing else with Podman.
- Copies from a transport that is not listed (`podman load` of an archive, a pull of a
  `docker-archive:` or `oci-archive:` name) are rejected by the default. An image already
  in local storage is not pulled again, so it still runs.
- Containerd and kubelet (for example under k3s) do not read this file.

Check the file with `podman image trust show`, which fails when the policy does not parse.
The restriction is only proven by a pull from a reference outside the scopes that is
refused; do that once on a host after changing the file.

Write the file as root and keep the previous one beside it, so it can be put back.

## `--container-pull-allow`

An agent that has `--container-pull-allow <prefix>` (repeatable) checks the image name
itself before it calls Podman, with the same registry-plus-namespace prefix as a scope
above, and refuses a job whose image is outside every prefix. Use the same list as the
scopes of `policy.json`: the agent then refuses before Podman is called, and the policy is
the second line. An agent that does not have the flag exits on it (as for any unknown flag),
so pass it only to a binary that documents it.
