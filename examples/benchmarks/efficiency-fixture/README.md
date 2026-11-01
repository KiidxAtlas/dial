# Arithmetic fixture

`launch_probe.py` is an unchanged execution fixture for `recovery.toml`. Its terminal mode
requires a real terminal; its pipe mode only requires stdin. Both require input and produce
a receipt only after receiving `confirm`. The independent checks verify the receipt and the
original probe hash. Scripted-provider runs exercise the harness, not model quality.
The launch cases use the explicit `managed_shell` benchmark mode; core isolation intentionally
does not include managed jobs. See `../launch-recovery-profiles.toml` for a local-only template.
