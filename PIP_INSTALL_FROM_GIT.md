# Installing graphdblite with pip directly from GitHub

The Python package lives in the `bindings/python` subdirectory of the repo, so pip
has to be pointed at that subdirectory:

```bash
pip install "git+https://github.com/arupkamal/graphdblite.git#subdirectory=bindings/python"
```

> The repo has no `pyproject.toml` at the top level, so
> `pip install git+https://github.com/arupkamal/graphdblite.git` (without
> `#subdirectory=...`) will fail. The original project's `docs/python.md` shows that
> plain form, so it's wrong upstream too.

pip clones the repo and compiles the Rust core on your machine.

---

## Prerequisites

- **Rust toolchain**: install from <https://rustup.rs>
- **git**
- **Python 3.10 or newer**

You don't need to install `maturin` yourself; pip fetches it automatically as the
build backend.

---

## Replace an existing PyPI install

If the PyPI version is already installed, uninstall it first so the old build isn't
the one Python imports:

```bash
pip uninstall graphdblite
pip install "git+https://github.com/arupkamal/graphdblite.git#subdirectory=bindings/python"
```

---

## Pin a specific commit (recommended)

Pinning means later pushes to the repo don't change what you install:

```bash
pip install "git+https://github.com/arupkamal/graphdblite.git@bf24f42#subdirectory=bindings/python"
```

Replace `bf24f42` with any commit hash, tag or branch name.

### In `requirements.txt`

```
graphdblite @ git+https://github.com/arupkamal/graphdblite.git@bf24f42#subdirectory=bindings/python
```

### In `pyproject.toml`

```toml
[project]
dependencies = [
  "graphdblite @ git+https://github.com/arupkamal/graphdblite.git@bf24f42#subdirectory=bindings/python",
]
```

---

## Check that you got the patched build

```python
from graphdblite import Database

db = Database("test.db")
db.execute("CREATE (:Person {name: 'Alice'})")
print(db.query("MATCH (n:person) RETURN count(n) AS c"))
# patched:  [{'c': 0}]
# original: [{'c': 1}]   <- old build: label matching ignored case
db.close()
```

---

## Notes

- **The first install takes a few minutes** while Rust compiles the core.
- **Every install clones the whole repo.** Removing the committed `targets/` build
  folder (about 388 MB) speeds this up:
  ```bash
  git rm -r --cached targets
  echo "/targets" >> .gitignore
  git add .gitignore
  git commit -m "Remove build artifacts; ignore /targets"
  git push
  ```
- **Installing on several machines or in CI?** Build a wheel once so the other
  machines don't need Rust:
  ```bash
  pip install maturin
  maturin build --release --manifest-path bindings/python/Cargo.toml
  # produces target/wheels/graphdblite-*.whl
  pip install target/wheels/graphdblite-*.whl
  ```
  You can attach the `.whl` file to a GitHub release and install it from its URL.

---

## Troubleshooting

- **`error: can't find Rust compiler` / `cargo not found`**: install Rust from
  <https://rustup.rs>, open a new terminal, and retry.
- **`does not appear to be a Python project`**: the `#subdirectory=bindings/python`
  part is missing from the URL.
- **The check above still prints `[{'c': 1}]`**: the PyPI build is being imported. Run
  `pip uninstall graphdblite` (repeat until nothing is left), then reinstall from git.

See `PYTHON_PATCHED_BUILD.md` for the behaviour changes that affect Python code
(case-insensitive full-text search and the queries that now raise errors).
