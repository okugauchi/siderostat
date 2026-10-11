from pathlib import Path
import shutil, subprocess, sys

checkout = Path(sys.argv[1]).resolve()
harness = Path(sys.argv[2]).resolve()
expected_commit = sys.argv[3]
actual = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
if actual != expected_commit:
    raise SystemExit(f"source commit mismatch: {actual}")
up = checkout / "codex-rs"
(harness / "src").mkdir(parents=True, exist_ok=True)
source = Path(__file__).with_name("main.rs")
shutil.copyfile(source, harness / "src/main.rs")
patch = (up / "Cargo.toml").read_text().split("[patch.crates-io]", 1)[1]
import json
manifest = "[package]\nname = \"codex-replay\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n"
manifest += "codex-protocol = { path = " + json.dumps(str(up / "protocol")) + " }\nserde_json = \"1\"\n\n[profile.dev]\ndebug = 0\n\n[patch.crates-io]" + patch
(harness / "Cargo.toml").write_text(manifest)
shutil.copyfile(up / "Cargo.lock", harness / "Cargo.lock")
subprocess.run(["cargo", "build", "--manifest-path", str(harness / "Cargo.toml"), "-j", "4"], check=True)
print(harness / "target/debug/codex-replay")
