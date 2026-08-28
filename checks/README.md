# Headless integration checks

`headless-session.sh` starts cbar inside a private, headless Wayland session. It proves two
layer-surface contracts that unit tests cannot observe:

- an internally dismissed resident launcher stays hidden after its next asynchronous preparation;
- the center container remains physically centred when start and end content have deliberately
  asymmetric widths.

The launcher assertion uses its opt-in progress trace plus the ordinary IPC status command. The
layout assertion uses `CBAR_LAYOUT_TRACE=1` to read GTK's final widget allocations, avoiding a
font- and theme-sensitive screenshot comparison. Neither trace is active in an ordinary session.

The harness does not install or pin a compositor. Pass an exact compositor argv after `--`; an
argument equal to `{config}` is replaced with the generated Sway-compatible fixture config:

```sh
checks/headless-session.sh target/debug/ironbar wtype -- scroll -c '{config}'
```

All launcher inventory, configuration, mutable state, caches, IPC, D-Bus, and Wayland sockets live
under one private temporary directory. `HOME` and inherited compositor socket variables are removed
before startup, so the check cannot read personal configuration or target the caller's graphical
session.
