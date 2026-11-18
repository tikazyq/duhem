"""Exercise real binaries. Run via the adjacent worked Verification Definition."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    duhem, dashboard = sys.argv[1:]
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        source, target = root / "source.db", root / "hub.db"
        fixture = root / "fixture.yml"
        fixture.write_text("verification: ingest-fixture\ncriteria:\n  - id: PASS\n    description: The assertion passes.\n    checks:\n      - id: pass\n        assertions: ['true']\n")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        environment = dict(os.environ, DUHEM_HUB_TOKEN="example-token")
        # The fixture is a separate verification act. Nested-run context belongs
        # to this worked definition and must not point it into the outer store.
        for name in ("DUHEM_PARENT_RUN_ID", "DUHEM_PARENT_DB_PATH", "DUHEM_SHARD", "DUHEM_RUN_ID"):
            environment.pop(name, None)
        server = subprocess.Popen([dashboard, "--ingest", "--db", str(target), "--port", str(port)], env=environment, stdout=subprocess.DEVNULL)
        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise RuntimeError("dashboard exited before readiness")
                try:
                    with urllib.request.urlopen(base + "/api/runs", timeout=1) as response:
                        if response.status == 200:
                            break
                except urllib.error.URLError:
                    time.sleep(0.05)
            else:
                raise RuntimeError("dashboard did not become ready")
            environment["DUHEM_HUB_URL"] = base + "/api/ingest"
            result = subprocess.run([duhem, "run", str(fixture), "--db", str(source), "--no-env-file", "--no-live", "--reporter", "json"], env=environment, capture_output=True, text=True, timeout=45, check=True)
            assert "automatic run delivery failed" not in result.stderr, result.stderr
            run_id = json.loads(result.stdout)["run_id"]
            with urllib.request.urlopen(base + "/api/runs/" + run_id, timeout=5) as response:
                assert response.status == 200
            subprocess.run([duhem, "ship", run_id, "--db", str(source)], env=environment, check=True, timeout=15)
            with urllib.request.urlopen(base + "/api/runs", timeout=5) as response:
                runs = json.load(response)
            assert len(runs) == 1, runs
            assert runs[0]["run_id"] == run_id, runs
            print("delivered and deduplicated")
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


if __name__ == "__main__":
    main()
