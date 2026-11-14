#!/bin/sh
# The forge the test plugin talks to. Pull request 1 streams three files in
# pieces, 2 never stops (it notes its pid in `pids` beside this script), 3 isn't
# signed in, and a number of 1000 or more streams that many lines of diff.
here=$(dirname "$0")
case "$1" in
  hosts) echo '["git.corp.example"]' ;;
  pulls) echo '[{"number":7,"url":"https://testforge.invalid/team/app/pull/7","title":"Stream it","head":"feature","base":"main","headSha":"","state":"open"}]' ;;
  view)
    if [ "$3" = 3 ]; then echo "not logged in to $TESTFORGE_HOST" >&2; exit 4; fi
    printf '{"number":%s,"title":"Stream it","author":"ada","state":"open","base":"main","head":"feature","additions":3,"deletions":1,"changedFiles":3,"url":"https://%s/%s/pull/%s"}\n' "$3" "$TESTFORGE_HOST" "$2" "$3"
    ;;
  commits)
    echo '[{"oid":"aaa111","author":"ada","headline":"First","authoredAt":"2026-11-01T10:00:00Z"},{"oid":"bbb222","author":"ada","headline":"Second","authoredAt":"2026-11-02T10:00:00Z"}]'
    ;;
  commit)
    printf '{"files":[{"path":"one.txt","status":"added","patch":"@@ -0,0 +1 @@\\n+%s"}],"details":{"message":"First\\n\\nIts body","author":"ada","email":"ada@example.com","date":"2026-11-01T10:00:00Z"}}\n' "$3"
    ;;
  diff)
    case "$3" in
      2)
        echo $$ >> "$here/pids"
        i=0
        while true; do
          printf 'diff --git a/f%s.txt b/f%s.txt\n--- a/f%s.txt\n+++ b/f%s.txt\n@@ -1 +1 @@\n-old\n+new\n' $i $i $i $i
          i=$((i+1))
          sleep 0.1
        done
        ;;
      3) echo "not logged in to $TESTFORGE_HOST" >&2; exit 4 ;;
      [0-9][0-9][0-9][0-9]*)
        lines=$3
        awk -v lines="$lines" 'BEGIN {
          per = 100; files = int(lines / per);
          for (f = 0; f < files; f++) {
            printf "diff --git a/src/f%d.rs b/src/f%d.rs\n--- a/src/f%d.rs\n+++ b/src/f%d.rs\n@@ -1,%d +1,%d @@\n", f, f, f, f, per / 2, per / 2;
            for (l = 0; l < per / 2; l++) { printf "-old line %d of file %d\n+new line %d of file %d\n", l, f, l, f }
          }
        }'
        ;;
      *)
        printf 'diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1,2 @@\n-was\n+is\n'
        sleep 0.2
        printf '+also\ndiff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n'
        sleep 0.2
        printf '@@ -0,0 +1 @@\n+bee\ndiff --git a/c.txt b/c.txt\ndeleted file mode 100644\n--- a/c.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-sea\n'
        ;;
    esac
    ;;
  *) echo "testforge: no such command: $1" >&2; exit 2 ;;
esac
