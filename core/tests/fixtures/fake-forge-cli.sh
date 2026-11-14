#!/bin/sh
# A stand-in for `gh` / `glab` in tests: never reaches a forge.
#
# Copied next to a `rules` file. Every call appends its host variable and its
# arguments to `calls` (each field NUL-terminated, each call ended by \001 and
# a newline). Then the first rule whose glob matches the arguments (joined by
# spaces) answers: a rule line is `glob<TAB>response-file<TAB>exit-code`, and
# the response goes to stdout on exit 0, to stderr otherwise. A call's record
# is written whole, in one append, so calls running at once don't interleave.
dir=$(dirname "$0")
record="$dir/call.$$"
{
  printf 'HOST=%s\0' "${GH_HOST:-$GITLAB_HOST}"
  for arg in "$@"; do printf '%s\0' "$arg"; done
  printf '\001\n'
} > "$record"
cat "$record" >> "$dir/calls"
rm -f "$record"
all="$*"
while IFS='	' read -r pattern response status; do
  case "$all" in
    $pattern)
      if [ "${status:-0}" = 0 ]; then
        cat "$dir/$response"
      else
        cat "$dir/$response" >&2
      fi
      exit "${status:-0}"
      ;;
  esac
done < "$dir/rules"
echo "fake forge cli: no rule for: $all" >&2
exit 97
