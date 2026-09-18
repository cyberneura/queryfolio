"""Fabric tasks for Queryfolio (Fabric3 / Fabric 1.x API).

Run `fab -l` to list tasks.

The release build (macOS dmg + Windows NSIS installer) runs on GitHub Actions and
is triggered manually from here with `fab release` (the Actions workflow is
workflow_dispatch only). That task just wraps `pnpm release`, which bumps the
version, pushes it to main, dispatches the workflow, and follows the run.
"""
import os

from fabric.api import task, local, lcd

PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


@task
def dev():
    """Start the app in development mode (Rust build + Vite + native window)."""
    with lcd(PROJECT_ROOT):
        local('pnpm tauri dev')


@task
def check():
    """Type-check the frontend (svelte-check) and run the Rust unit tests."""
    with lcd(PROJECT_ROOT):
        local('pnpm check')
    with lcd(os.path.join(PROJECT_ROOT, 'src-tauri')):
        local('cargo test')


# Alias so `fab unittest` works (the standard task name in the fabfile skill).
@task
def unittest():
    """Run the Rust unit tests."""
    with lcd(os.path.join(PROJECT_ROOT, 'src-tauri')):
        local('cargo test')


@task
def build_local():
    """Build a signed release on this Mac (Developer ID, not notarized)."""
    with lcd(PROJECT_ROOT):
        local('pnpm tauri build')


@task
def release(bump='patch'):
    """Bump the version and run the release build on GitHub Actions.

        fab release              # 0.1.0 -> 0.1.1 (patch)
        fab release:minor        # 0.1.0 -> 0.2.0
        fab release:major        # 0.1.0 -> 1.0.0

    Thin wrapper around `pnpm release` (scripts/release.sh): it bumps the version
    in tauri.conf.json / package.json, commits and pushes to main (the push starts
    the Release workflow), and follows the run. The workflow builds the macOS universal
    dmg (Developer ID signed + notarized) and the Windows NSIS installer, then
    publishes the draft Release once every platform succeeded. See the
    `publish-macos-release` skill for the full runbook.
    """
    if bump not in ('patch', 'minor', 'major'):
        raise SystemExit(
            "bump must be 'patch', 'minor' or 'major', got: {!r}".format(bump)
        )
    with lcd(PROJECT_ROOT):
        local('pnpm release {}'.format(bump))


@task
def releases():
    """List the GitHub Releases (published builds)."""
    with lcd(PROJECT_ROOT):
        local('gh release list')
