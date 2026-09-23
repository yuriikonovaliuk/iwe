# iwe-plus

This repository is **iwe-plus**, a fork of [iwe](https://github.com/iwe-org/iwe)
that adds transactions, a commit journal and trigger, schema and invariant
validation, external checkers, frozen/immutable properties, a shared MCP
daemon and the hooks the knowledge compositor (kc) builds on.

## Versions

iwe-plus has its own [semver](https://semver.org) line, starting at 1.0.0
(based on upstream iwe 0.24.2). Every binary reports both, and the commit:

```
$ iwe --version
iwe 1.0.0 (iwe-plus; upstream iwe 0.24.2; 60efc92)
```

- `Cargo.toml` `[workspace.package] version` is the iwe-plus version.
- `UPSTREAM_VERSION` is the upstream release last merged; bump it with every
  upstream merge.
- A release is a `vX.Y.Z` tag on master. A breaking change to the CLI, the
  MCP tools, the configuration or the journal/commit-trigger contract is a
  major bump; an upstream merge is at least a minor one.

The binaries keep upstream's names (`iwe`, `iwec`, `iwes`) so tools, editors
and the knowledge compositor find them unchanged.

## Nix

```
nix build github:yuriikonovaliuk/iwe          # iwe, iwec, iwes
nix profile install github:yuriikonovaliuk/iwe/v1.0.0
```
