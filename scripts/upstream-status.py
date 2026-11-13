#!/usr/bin/env python3
"""How far each OxideAV fork this workspace pins is from upstream OxideAV.

For every `oxideav-*` crate the root Cargo.toml pins to an ayooooo123 fork,
prints how many commits upstream's default branch has that the pin lacks,
how many the pin has that upstream lacks, and (with --merge) how many files a
trial merge of upstream into the pin conflicts in. Needs `gh` (logged in) and
`git`. Trial merges run in throwaway blobless clones under --work.

    python3 scripts/upstream-status.py                 # counts only
    python3 scripts/upstream-status.py --merge         # plus trial merges
    python3 scripts/upstream-status.py --markdown out.md
"""
import argparse, json, pathlib, re, subprocess, sys, tempfile

root = pathlib.Path(__file__).resolve().parent.parent
PIN = re.compile(r'^(oxideav-[a-z0-9-]+) = \{ git = "https://github.com/ayooooo123/([a-z0-9-]+)", rev = "([0-9a-f]+)" \}', re.M)


def gh(path, jq=None):
    cmd = ["gh", "api", path] + (["--jq", jq] if jq else [])
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip() or f"gh api {path} failed")
    text = out.stdout.strip()
    try:
        return json.loads(text)
    except json.JSONDecodeError:  # `--jq` on a string field prints it bare
        return text


def trial_merge(work, repo, rev, branch):
    clone = work / repo
    git = lambda *a: subprocess.run(["git", "-C", str(clone), *a], capture_output=True, text=True)
    if not clone.is_dir():
        subprocess.run(["git", "clone", "-q", "--filter=blob:none",
                        f"https://github.com/ayooooo123/{repo}", str(clone)], check=True)
        git("remote", "add", "upstream", f"https://github.com/OxideAV/{repo}")
    git("fetch", "-q", "origin")
    git("fetch", "-q", "upstream", branch)
    git("checkout", "-q", "-f", rev)
    git("merge", "--no-commit", "--no-ff", "-q", f"upstream/{branch}")
    conflicted = [f for f in git("diff", "--name-only", "--diff-filter=U").stdout.split() if f]
    git("merge", "--abort")
    git("reset", "-q", "--hard", rev)
    return conflicted


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--merge", action="store_true", help="also trial-merge upstream into each pin")
    ap.add_argument("--work", type=pathlib.Path, help="where trial-merge clones live (default: a temp dir)")
    ap.add_argument("--markdown", type=pathlib.Path, help="also write the table as Markdown here")
    args = ap.parse_args()

    pins = PIN.findall((root / "Cargo.toml").read_text())
    work = args.work or pathlib.Path(tempfile.mkdtemp(prefix="oxideav-upstream-"))
    rows = []
    for crate, repo, rev in sorted(pins):
        try:
            branch = gh(f"repos/OxideAV/{repo}", ".default_branch")
            cmp = gh(f"repos/OxideAV/{repo}/compare/{rev}...{branch}", "{ahead_by,behind_by}")
            conflicts = trial_merge(work, repo, rev, branch) if args.merge else None
            rows.append((crate, rev[:9], branch, cmp["ahead_by"], cmp["behind_by"], conflicts, None))
        except Exception as err:  # one unreachable repo must not hide the rest
            rows.append((crate, rev[:9], "?", None, None, None, str(err)[:80]))
        print(f"{crate}: done", file=sys.stderr)

    head = ["crate", "pin", "upstream branch", "upstream has", "we have", "merge conflicts"]
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for crate, pin, branch, up, ours, conflicts, err in rows:
        merge = "—" if conflicts is None else ("clean" if not conflicts else f"{len(conflicts)} files")
        cells = [crate, pin, branch, err or str(up), "" if err else str(ours), merge]
        lines.append("| " + " | ".join(cells) + " |")
    total_up = sum(r[3] or 0 for r in rows)
    total_ours = sum(r[4] or 0 for r in rows)
    lines.append(f"\n{len(rows)} forks; upstream has {total_up} commits we lack, we have {total_ours} it lacks.")
    table = "\n".join(lines)
    print(table)
    if args.markdown:
        args.markdown.write_text(table + "\n")


if __name__ == "__main__":
    main()
