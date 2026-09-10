"""Exercise the Bash entrypoint with a fake network and isolated user directory."""
from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import zipfile

import pytest


INSTALLER = Path(__file__).resolve().parents[1] / "install.sh"
UV_SHA = "d0fec58f3124e05e0a1af0f6541abfce4333253cdaf23c7b6bb2e6128bf138ea"
RELEASE_API = "https://api.github.com/repos/ayushjainr/pikamux/releases?per_page=100"


def release_row(version, **overrides):
    return {"tag_name": f"v{version}", "draft": False, "prerelease": True,
            "assets": [{"name": name, "state": "uploaded"} for name in
                       ("pika-release.json", f"pikamux-{version}-py3-none-any.whl")], **overrides}


def executable(path: Path, contents: str) -> None:
    path.write_text(contents, encoding="utf-8")
    path.chmod(0o755)


@pytest.fixture
def bootstrap(tmp_path):
    fake = tmp_path / "bin"
    fake.mkdir()
    temporary = tmp_path / "temp"
    temporary.mkdir()
    bundle = tmp_path / "private bundle"
    bundle.mkdir()
    wheel = bundle / "pikamux-0.5.0a1-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("pikamux/__init__.py", "")
        archive.writestr("pikamux/installation.py", "import json,os,sys\nfrom pathlib import Path\nPath(os.environ['PIKA_TEST_TRACE']).write_text(json.dumps(sys.argv[1:]))\n")
    manifest = {"schema": 1, "version": "0.5.0a1", "wheel": wheel.name, "sha256": hashlib.sha256(wheel.read_bytes()).hexdigest()}
    (bundle / "pika-release.json").write_text(json.dumps(manifest))
    (bundle / "releases.json").write_text(json.dumps([release_row("0.5.0a1")]))
    trace = tmp_path / "invoked.json"
    network = tmp_path / "network.log"
    uv_archive = tmp_path / "fake-uv.tar.gz"
    uv_script = f'''#!{sys.executable}
import json,os,sys
from pathlib import Path
if os.environ.get('PIKA_TEST_RUNTIME_ENV'):
    Path(os.environ['PIKA_TEST_RUNTIME_ENV']).write_text(json.dumps(dict(os.environ)))
args=' '.join(sys.argv[1:])
if 'python find' in args: print({sys.executable!r})
elif 'python install' not in args: sys.exit(90)
'''.encode()
    with tarfile.open(uv_archive, "w:gz") as archive:
        info = tarfile.TarInfo("uv-x86_64-unknown-linux-musl/uv")
        info.size = len(uv_script)
        info.mode = 0o755
        archive.addfile(info, io.BytesIO(uv_script))
    executable(fake / "uname", '#!/bin/sh\nif [ "$1" = "-s" ]; then echo Linux; else echo x86_64; fi\n')
    executable(fake / "curl", f'''#!{sys.executable}
import os,shutil,sys
from pathlib import Path
args=sys.argv[1:]; url=args[-1]; output=args[args.index('--output')+1]
with open(os.environ['PIKA_TEST_NETWORK'],'a') as stream: stream.write(url+'\\n')
if os.environ.get('PIKA_TEST_NETWORK_FAIL'): sys.exit(22)
source=Path(os.environ['PIKA_TEST_UV_ARCHIVE']) if '/astral-sh/uv/' in url else Path(os.environ['PIKA_TEST_BUNDLE'])/('releases.json' if url == {RELEASE_API!r} else url.rsplit('/',1)[-1])
shutil.copyfile(source, output)
''')
    executable(fake / "sha256sum", f'''#!{sys.executable}
import hashlib,os,sys
from pathlib import Path
path=Path(sys.argv[1])
value=('0'*64 if os.environ.get('PIKA_TEST_BAD_UV') else '{UV_SHA}') if path.name=='uv.tar.gz' else hashlib.sha256(path.read_bytes()).hexdigest()
print(value+'  '+str(path))
''')
    env = {**os.environ, "PATH": f"{fake}:/usr/bin:/bin", "HOME": str(tmp_path / "home"), "TMPDIR": str(temporary), "PIKA_TEST_TRACE": str(trace), "PIKA_TEST_NETWORK": str(network), "PIKA_TEST_UV_ARCHIVE": str(uv_archive), "PIKA_TEST_BUNDLE": str(bundle)}

    def run(*args, **overrides):
        return subprocess.run(["/bin/bash", str(INSTALLER), *args], env={**env, **overrides}, capture_output=True, text=True, timeout=15)
    return run, bundle, trace, network, temporary


def test_help_never_contacts_network(bootstrap):
    run, _, trace, network, temporary = bootstrap
    result = run("--help")
    assert result.returncode == 0
    assert "--bundle" in result.stdout
    assert not network.exists() and not trace.exists()
    assert not list(temporary.iterdir())


def test_local_bundle_verifies_and_forwards_install_arguments(bootstrap, tmp_path):
    run, bundle, trace, network, temporary = bootstrap
    result = run("--bundle", str(bundle), "--root", str(tmp_path / "Pika Root"), "--bin-dir", str(tmp_path / "Pika Bin"), "--no-setup")
    assert result.returncode == 0, result.stderr
    args = json.loads(trace.read_text())
    assert args[0] == "install"
    assert args[args.index("--root") + 1] == str(tmp_path / "Pika Root")
    assert args[args.index("--bin-dir") + 1] == str(tmp_path / "Pika Bin")
    assert "--no-setup" in args
    assert len(args[args.index("--sha256") + 1]) == 64
    assert all("/astral-sh/uv/" in url for url in network.read_text().splitlines())
    assert not list(temporary.iterdir())


def test_published_version_downloads_only_fixed_repository_assets(bootstrap):
    run, _, trace, network, _ = bootstrap
    result = run("--version", "v0.5.0a1", "--no-setup")
    assert result.returncode == 0, result.stderr
    urls = network.read_text().splitlines()
    assert urls[0] == "https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a1/pika-release.json"
    assert urls[-1] == "https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a1/pikamux-0.5.0a1-py3-none-any.whl"
    assert trace.exists(), (result.stdout, result.stderr)


def test_default_install_works_with_empty_optional_arguments(bootstrap):
    run, _, trace, network, _ = bootstrap
    result = run()
    assert result.returncode == 0, result.stderr
    assert trace.exists(), (result.stdout, result.stderr)
    urls = network.read_text().splitlines()
    assert urls[0] == RELEASE_API
    assert urls[-2:] == [
        "https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a1/pika-release.json",
        "https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a1/pikamux-0.5.0a1-py3-none-any.whl",
    ]


@pytest.mark.parametrize("versions, expected", [
    (["0.5.0a9", "0.5.0a10", "0.5.0a2"], "0.5.0a10"),
    (["0.5.0rc1", "0.5.0", "0.5.0b9"], "0.5.0"),
    (["0.9.0", "0.10.0a1", "0.8.0"], "0.10.0a1"),
])
def test_default_selects_numeric_newest_not_listing_order(bootstrap, versions, expected):
    run, bundle, trace, network, _ = bootstrap
    original = bundle / "pikamux-0.5.0a1-py3-none-any.whl"
    manifest = json.loads((bundle / "pika-release.json").read_text())
    wheel = f"pikamux-{expected}-py3-none-any.whl"
    (bundle / wheel).write_bytes(original.read_bytes())
    (bundle / "pika-release.json").write_text(json.dumps({**manifest, "version": expected, "wheel": wheel}))
    (bundle / "releases.json").write_text(json.dumps([release_row(v) for v in versions]))
    result = run()
    assert result.returncode == 0, result.stderr
    assert trace.exists()
    assert f"/download/v{expected}/{wheel}" in network.read_text()


def test_default_ignores_drafts_incomplete_and_unsafe_releases(bootstrap):
    run, bundle, trace, network, _ = bootstrap
    rows = [release_row("99.0.0", draft=True), release_row("98.0.0", assets=[]),
            release_row("97.0.0", assets=[{"name": "pika-release.json", "state": "uploaded"},
                                          {"name": "pikamux-97.0.0-py3-none-any.whl", "state": "new"}]),
            release_row("0.5.0a1"), release_row("../../unsafe"), None]
    (bundle / "releases.json").write_text(json.dumps(rows))
    result = run()
    assert result.returncode == 0, result.stderr
    assert trace.exists()
    assert "/download/v0.5.0a1/" in network.read_text()
    assert "unsafe" not in network.read_text()


@pytest.mark.parametrize("listing", ["[]", "{}", "not json", '[{"draft": false}]'])
def test_default_bad_listing_never_activates(bootstrap, listing):
    run, bundle, trace, network, temporary = bootstrap
    (bundle / "releases.json").write_text(listing)
    result = run()
    assert result.returncode != 0
    assert "Cannot select a complete published release" in result.stderr
    assert not trace.exists() and not list(temporary.iterdir())
    assert not any("/pikamux/releases/download/" in url for url in network.read_text().splitlines())


def test_default_pins_manifest_to_selected_release(bootstrap):
    run, bundle, trace, _, _ = bootstrap
    (bundle / "releases.json").write_text(json.dumps([release_row("0.5.0a2")]))
    result = run()
    assert result.returncode != 0
    assert "requested-version mismatch" in result.stderr
    assert not trace.exists()


@pytest.mark.parametrize("change", [{"wheel": "../../payload.whl"}, {"wheel": "https://evil.example/payload.whl"}, {"version": "../x"}, {"schema": True}, {"sha256": "x" * 64}])
def test_untrusted_manifest_fields_never_execute_package(bootstrap, change):
    run, bundle, trace, _, temporary = bootstrap
    path = bundle / "pika-release.json"
    manifest = json.loads(path.read_text())
    path.write_text(json.dumps({**manifest, **change}))
    result = run("--bundle", str(bundle))
    assert result.returncode != 0
    assert "Invalid release manifest" in result.stderr
    assert not trace.exists()
    assert not list(temporary.iterdir())


def test_wheel_checksum_failure_does_not_execute_package(bootstrap):
    run, bundle, trace, _, temporary = bootstrap
    (bundle / "pikamux-0.5.0a1-py3-none-any.whl").write_bytes(b"tampered")
    result = run("--bundle", str(bundle))
    assert result.returncode != 0
    assert "Pika checksum mismatch" in result.stderr
    assert not trace.exists() and not list(temporary.iterdir())


def test_uv_checksum_failure_prevents_runtime_execution(bootstrap):
    run, bundle, trace, _, temporary = bootstrap
    result = run("--bundle", str(bundle), PIKA_TEST_BAD_UV="1")
    assert result.returncode != 0
    assert "uv checksum mismatch" in result.stderr
    assert not trace.exists() and not list(temporary.iterdir())


def test_unavailable_private_release_has_actionable_error(bootstrap):
    run, _, trace, _, temporary = bootstrap
    result = run(PIKA_TEST_NETWORK_FAIL="1")
    assert result.returncode != 0
    assert "--bundle DIRECTORY" in result.stderr
    assert not trace.exists() and not list(temporary.iterdir())


@pytest.mark.parametrize("args", [("--version", "../../escape"), ("--version",), ("--unknown",)])
def test_invalid_arguments_fail_before_network(bootstrap, args):
    run, _, _, network, _ = bootstrap
    assert run(*args).returncode != 0
    assert not network.exists()


def test_requested_version_must_match_manifest(bootstrap):
    run, _, trace, _, temporary = bootstrap
    result = run("--version", "v99.0.0")
    assert result.returncode != 0
    assert "requested-version mismatch" in result.stderr
    assert not trace.exists() and not list(temporary.iterdir())


def test_runtime_environment_is_sanitized_before_uv_runs(bootstrap, tmp_path):
    run, bundle, trace, _, _ = bootstrap
    observed = tmp_path / "runtime-environment.json"
    unsafe = {"UV_PYTHON_INSTALL_DIR": "/unrelated/python", "UV_PYTHON_INSTALL_MIRROR": "https://untrusted.invalid/python", "UV_INSECURE_HOST": "*", "PIP_INDEX_URL": "https://untrusted.invalid/packages", "PIP_TARGET": "/unrelated/site-packages", "PYTHONPATH": "/unrelated/modules", "PYTHONHOME": "/nonexistent/python", "PYTHONSTARTUP": "echo injected", "VIRTUAL_ENV": "/unrelated/venv", "CONDA_PREFIX": "/unrelated/conda"}
    preserved = {"HTTPS_PROXY": "http://proxy.invalid:3128", "SSL_CERT_FILE": "/private/company-ca.pem"}
    result = run("--bundle", str(bundle), PIKA_TEST_RUNTIME_ENV=str(observed), **unsafe, **preserved)
    assert result.returncode == 0, result.stderr
    runtime = json.loads(observed.read_text())
    assert not any(name in runtime for name in unsafe)
    assert all(runtime.get(name) == value for name, value in preserved.items())
    assert runtime["HOME"] == str(tmp_path / "home")
    assert trace.exists()


def test_unprefixed_release_version_uses_canonical_v_tag(bootstrap):
    run, _, trace, network, _ = bootstrap
    result = run("--version", "0.5.0a1")
    assert result.returncode == 0, result.stderr
    assert "/download/v0.5.0a1/pika-release.json" in network.read_text()
    assert trace.exists()


@pytest.mark.parametrize("version", ["1.0", "1.0.0dev1", "1.0.0+local", "v1.0.0.post1"])
def test_unsupported_release_versions_fail_before_network(bootstrap, version):
    run, _, _, network, _ = bootstrap
    result = run("--version", version)
    assert result.returncode != 0
    assert "Invalid release tag" in result.stderr
    assert not network.exists()


def test_oversized_manifest_fails_before_runtime_download(bootstrap):
    run, bundle, trace, network, temporary = bootstrap
    manifest = bundle / "pika-release.json"
    manifest.write_text(manifest.read_text() + " " * 65536)
    result = run("--bundle", str(bundle))
    assert result.returncode != 0
    assert "64 KiB limit" in result.stderr
    assert not network.exists() and not trace.exists()
    assert not list(temporary.iterdir())


@pytest.mark.timeout(300)
@pytest.mark.parametrize("source", ["bundle", "published"])
def test_real_fresh_machine_bootstrap_when_bundle_is_supplied(tmp_path, source):
    """Opt-in network test: no installed uv/Python on PATH and no real-home writes.

    PIKA_BOOTSTRAP_BUNDLE points to a freshly built, trusted local bundle.
    PIKA_BOOTSTRAP_PUBLIC_VERSION enables the no-version public install and
    specifies the expected result for the assertion, not an installer argument.
    uv, its managed Python, and package dependencies download into this test's
    isolated home. The normal test suite never downloads anything here.
    """
    supplied = os.environ.get("PIKA_BOOTSTRAP_BUNDLE" if source == "bundle" else "PIKA_BOOTSTRAP_PUBLIC_VERSION")
    if not supplied:
        pytest.skip("Set PIKA_BOOTSTRAP_BUNDLE or PIKA_BOOTSTRAP_PUBLIC_VERSION for isolated network tests")
    source_args = []
    expected_version = supplied
    if source == "bundle":
        bundle = Path(supplied).resolve()
        assert bundle.is_dir() and (bundle / "pika-release.json").is_file()
        expected_version = json.loads((bundle / "pika-release.json").read_text())["version"]
        source_args = ["--bundle", str(bundle)]
    isolated_home = tmp_path / "home"
    isolated_home.mkdir()
    temporary = tmp_path / "temp"
    temporary.mkdir()
    root, bin_dir = tmp_path / "install", tmp_path / "bin"
    env = {"HOME": str(isolated_home), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "TMPDIR": str(temporary), "SHELL": "/bin/bash"}
    for name in ("HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "all_proxy", "no_proxy", "SSL_CERT_FILE", "SSL_CERT_DIR", "CURL_CA_BUNDLE"):
        if name in os.environ:
            env[name] = os.environ[name]
    result = subprocess.run(["/bin/bash", str(INSTALLER), *source_args, "--root", str(root), "--bin-dir", str(bin_dir), "--no-setup"], env=env, cwd=tmp_path, capture_output=True, text=True, timeout=240)
    assert result.returncode == 0, (result.stdout, result.stderr)
    assert (root / "tools/uv").is_file()
    command = bin_dir / "pika"
    assert command.is_file()
    version = subprocess.run([str(command), "--version"], env=env, cwd=tmp_path, capture_output=True, text=True, timeout=20)
    assert version.returncode == 0, version.stderr
    assert expected_version in version.stdout
    managed_python = root / "current/bin/python"
    assert managed_python.exists()
    # The venv's base Python is persistent but belongs to the temporary home,
    # not to the real user's uv installation or a soon-deleted bootstrap dir.
    assert managed_python.resolve().is_relative_to(isolated_home.resolve())
    assert not list(temporary.glob("pika-install.*"))
    assert not any((isolated_home / name).exists() for name in (".bashrc", ".bash_profile", ".profile", ".zshrc", ".zprofile"))
