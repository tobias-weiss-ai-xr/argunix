# Container images

argunix can build OCI/Docker container images straight from your flake,
push them to a registry, and attach a software bill of materials (SBOM)
— with no CI YAML and no push scripts. You declare the image as a normal
flake output and tag it with one `meta` attribute; argunix does the
rest.

This guide is for **flake authors**. The registry itself is wired up by
whoever runs argunix — see [Prerequisites](#prerequisites).

## The one knob: `meta.image-format`

argunix treats a package output as a container image when its `meta`
carries an `image-format`:

```nix
pkgs.dockerTools.buildLayeredImage {
  name = "hello";
  tag = "latest";
  contents = [ pkgs.hello ];
  config.Cmd = [ "/bin/hello" ];
} // {
  meta.image-format = "docker";   # ← this line opts the output in
}
```

Without `meta.image-format`, the output is built like any other package
(no registry push, no SBOM). With it, argunix pushes the built image and
attaches its SBOM.

Two values are accepted:

- **`docker`** — a single-architecture image, the output of
  `dockerTools.buildLayeredImage` / `buildImage`. This is the format to
  use for multi-arch images (see below): you expose one `docker` output
  _per architecture_ and argunix stitches them together.
- **`oci`** — an already-complete image archive (an `oci-archive`,
  possibly _already_ multi-arch). argunix pushes it whole, untouched.

The registry coordinates do **not** come from the `name`/`tag` inside
`buildLayeredImage` — those are internal to the archive. The image name
is your **attribute name**, and the registry/namespace come from
argunix's config. `packages.x86_64-linux.hello` lands at
`<registry>/<namespace>/hello`.

## A single-arch image

Expose the image once, under the system you build for:

```nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      packages.x86_64-linux.hello = pkgs.dockerTools.buildLayeredImage {
        name = "hello";
        tag = "latest";
        contents = [ pkgs.hello ];
        config.Cmd = [ "/bin/hello" ];
      } // { meta.image-format = "docker"; };
    };
}
```

On every build argunix pushes this image to the registry and attaches a
CycloneDX SBOM as an OCI _referrer_ of the pushed manifest.

## A multi-arch image

Expose the **same attribute name once per architecture**. argunix builds
each on a builder of that system, then — with no extra configuration —
assembles the per-arch images into a single multi-arch OCI image index
on the registry:

```nix
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      forSystems = nixpkgs.lib.genAttrs [ "x86_64-linux" "aarch64-linux" ];
    in {
      packages = forSystems (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          hello = pkgs.dockerTools.buildLayeredImage {
            name = "hello";
            tag = "latest";
            contents = [ pkgs.hello ];
            config.Cmd = [ "/bin/hello" ];
          } // { meta.image-format = "docker"; };
        });
    };
}
```

This exposes `packages.x86_64-linux.hello` and
`packages.aarch64-linux.hello`. Because they share the logical name
`hello` and are both `docker`, argunix groups them and publishes one
image index — a consumer that pulls `hello:latest` automatically gets
the right architecture.

This is the point of per-arch `docker` outputs: each architecture is
built **natively** on its own builder, so software that cross-compiles
badly just works. (If yours cross-compiles cleanly, you can instead
build the other arch with `pkgs.pkgsCross.<target>.dockerTools…` and
still expose it under the per-system attribute — argunix does not care
_how_ each slice was built, only that it exists.)

If one architecture fails to build, argunix still assembles an index
from the architectures that succeeded and notes the missing one.

## `docker` vs `oci` — and the clash to avoid

| You have…                                             | Tag it          | argunix…                     |
| ----------------------------------------------------- | --------------- | ---------------------------- |
| one single-arch image                                 | `docker`        | pushes it as one manifest    |
| the same image, once per system                       | `docker` (each) | assembles a multi-arch index |
| one complete image archive (maybe already multi-arch) | `oci`           | pushes it whole, untouched   |

The rule: **`docker` = a slice argunix may assemble; `oci` = a finished
image argunix must not touch.**

Do **not** expose an `oci` image under the same attribute name across
several systems. argunix cannot tell whether each is a slice to merge or
a complete image, so it refuses the group with an errored status (a
"clash"). If you have one already-multi-arch image, expose it as a
single `oci` output for one system; if you want argunix to do the
merging, expose per-arch `docker` outputs.

## SBOMs

Every image — single- or multi-arch, `docker` or `oci` — gets a
CycloneDX SBOM, transcribed from the image's `/nix/store` runtime
closure and attached to the registry as an OCI referrer.

For a **multi-arch** image, each architecture gets its **own** SBOM,
attached to that platform's manifest digest — an `amd64` and an `arm64`
image genuinely ship different binaries, so two SBOMs is the honest
answer.

Inspect what argunix published, using only standard tools:

```sh
# Single-arch: the SBOM is a referrer of the image manifest.
oras discover <registry>/<namespace>/hello:sha-<short>

# Multi-arch: first see the platforms in the index…
skopeo inspect --raw docker://<registry>/<namespace>/hello:latest

# …then discover the SBOM of one platform's manifest by its digest.
oras discover <registry>/<namespace>/hello@sha256:<digest>

# Pull an SBOM document down to read it.
oras pull <registry>/<namespace>/hello@sha256:<sbom-digest>
```

The SBOM is also browsable in the argunix web UI, on each job's page.

### `meta.sbom-runtime-roots` (optional)

By default argunix derives the SBOM by scanning the store paths shipped
in the image's layers. If you want to pin it to an exact set of roots
instead, declare them:

```nix
… // {
  meta.image-format = "docker";
  meta.sbom-runtime-roots = [ "${pkgs.hello}" ];
};
```

argunix then takes the runtime closure of exactly those paths. Leave it
unset unless you have a specific reason — the layer scan needs no
upkeep.

## What lands on the registry

argunix tags each pushed image (or index) with:

- the **branch name** of the build (`main`, `feature-x`, …),
- **`latest`**, on builds of the repository's default branch,
- an immutable **`sha-<short>`** of the commit.

A pull-request build gets only the `sha-<short>` tag.

## Prerequisites

These are set by whoever operates argunix, not in your flake — but if
multi-arch isn't working, this is what to check with them.

- **Evaluated systems.** argunix only walks `packages.<system>.*` for
  the systems it is configured to evaluate (`services.argunix.systems`
  in the NixOS module). The default is the coordinator host's own
  system only — so a flake's `aarch64-linux` outputs are **silently
  ignored** until `aarch64-linux` is added there. This is the usual
  reason an architecture never shows up in the evaluation table.
- **Builders.** Each architecture is built on a builder of that system.
  Multi-arch therefore needs an `aarch64-linux` builder (native, or one
  with binfmt emulation) available to argunix; without one its jobs
  land in `SkippedNoBuilder`.
- **A registry binding.** Your repository must be bound to a registry in
  argunix's configuration (`push_to_registries`) — that, and the
  registry credentials, are set by whoever operates argunix. Ask them to
  add the binding, then your images publish on every build.
