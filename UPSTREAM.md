# Upstream relationship

This repository is derived from [Ironbar](https://github.com/JakeStanger/ironbar).
The fork began from Ironbar commit
`5b96bcffac54dd82347badcc07f79d58efa715c7` and preserves Ironbar's MIT
license and copyright notice.

## Compatibility contract

The fork keeps Ironbar's configuration formats, module configuration, styling
surface, extension points, and supported compositor scope. Fork-specific
features are additive. A release must pass the compatibility and regression
gates documented in `docs/Cbar architecture.md` before it is published.

Repository, package, application, and release identities are intentionally
separate from upstream ownership. The final public identity is not assigned in
this working tree yet. Every Cargo package therefore has `publish = false`;
publication is enabled only after the final package names, repository metadata,
and release ownership are assigned together.

## Synchronising upstream

The `upstream` remote is fetch-only. After fetching `upstream/master`, integrate
it on a dedicated branch, resolve conflicts without discarding either
upstream-supported behaviour or fork release gates, and run the complete
release matrix before merging. Published fork history is not rebased or
force-pushed merely to make it resemble upstream history.

Record the integrated upstream commit in each release. Never publish or push to
the official Ironbar repository from this working tree.
