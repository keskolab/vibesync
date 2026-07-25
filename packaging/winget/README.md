# winget submission

These manifests make VibeSync installable with `winget install VibeSync` —
no browser, no SmartScreen download block, no publisher warning dialog.

Submit AFTER the GitHub release is published (winget's validation
downloads the InstallerUrl, so it must be public):

1. Fork https://github.com/microsoft/winget-pkgs
2. Copy these three files to
   `manifests/j/keskolab.VibeSync/0.2.0/` in the fork
3. Open a PR — automated validation runs, a human moderator approves,
   and the package goes live in a few days.

Or use the tooling instead of a manual PR:
`wingetcreate submit` or `winget install wingetcreate` and point it at
the release URL — it builds and PRs the same manifests.

For each new release: bump PackageVersion, InstallerUrl, and
InstallerSha256 (`sha256sum` of the new setup.exe), then PR again —
or let `wingetcreate update keskolab.VibeSync` do it.
