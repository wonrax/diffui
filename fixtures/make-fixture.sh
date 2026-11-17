#!/usr/bin/env bash
# make-fixture.sh <dest>
#
# Builds a deterministic scratch jj repo at <dest> (must not exist): colocated
# git, one bare remote `origin` kept at <dest>/.jj/fixture-remote.git so deleting
# <dest> removes everything. Same change ids and commit ids on every run and at
# any path. Writes <dest>/.jj/fixture-manifest with `name change_id commit_id`.
# Operation ids do vary with <dest>: jj records the command line in each op.
#
# The app needs a jj identity to mutate (the one used here lives only in a
# temporary config): set JJ_CONFIG to a file with [user] name/email when running.
#
# Reach things by the bookmarks in the manifest, not by offsets. An empty-repo
# scene is just `jj git init <dir>`; nothing here covers it.
#
# Needs jj 0.45 to 0.47 ($JJ, which the dev shell sets) and git on PATH.
set -euo pipefail

dest=${1:?usage: make-fixture.sh <dest>}
[[ -e $dest ]] && { echo "make-fixture: $dest already exists" >&2; exit 1; }
JJ=${JJ:?set JJ to a jj 0.45 to 0.47 (the dev shell does)}
case "$("$JJ" --version)" in
  "jj 0.45."* | "jj 0.46."* | "jj 0.47."*) ;;
  *) echo "make-fixture: need jj 0.45.x to 0.47.x, got $("$JJ" --version)" >&2; exit 1 ;;
esac

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
export HOME=$scratch
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export JJ_CONFIG=$scratch/jj.toml
cat >"$JJ_CONFIG" <<'TOML'
[user]
name = "Ada Lovelace"
email = "ada@example.com"
[signing]
behavior = "drop"
[snapshot]
max-new-file-size = "32MiB"
[ui]
color = "never"
pager = ":builtin"
paginate = "never"
[git]
private-commits = "none()"
TOML
export JJ_OP_HOSTNAME=fixture JJ_OP_USERNAME=fixture
export TZ=UTC LC_ALL=C

tick=0
clock() {
  tick=$((tick + 1))
  local mins=$((tick * 37))
  local stamp
  stamp=$(printf '2026-03-%02dT%02d:%02d:00+00:00' \
    $((1 + mins / 1440)) $((mins / 60 % 24)) $((mins % 60)))
  export JJ_TIMESTAMP=$stamp JJ_OP_TIMESTAMP=$stamp JJ_RANDOMNESS_SEED=$((1000 + tick))
  export GIT_AUTHOR_DATE=$stamp GIT_COMMITTER_DATE=$stamp
}
who=("Ada Lovelace" ada@example.com)
as() { who=("$1" "$2"); }
ada() { as "Ada Lovelace" ada@example.com; }
bob() { as "Bob Martinez" bob@example.com; }
mei() { as "田中 美咲" mei@example.jp; }
long() { as "Maximiliana Wilhelmina von Hohenzollern-Sigmaringen the Third" maximiliana.wilhelmina@a-very-long-subdomain.example.org; }
j() {
  clock
  JJ_USER=${who[0]} JJ_EMAIL=${who[1]} "$JJ" --quiet "$@"
}
# start <parents...> -- <message>: new revision; following file writes land in it.
start() {
  local parents=()
  while [[ $1 != -- ]]; do parents+=("$1"); shift; done
  shift
  j new "${parents[@]}" -m "$1"
}
snap() { j status >/dev/null; }
push_new() {
  local name
  for name in "$@"; do
    j bookmark track "$name" --remote origin
    j git push -b "$name"
  done
}

j git init --colocate "$dest" 2>/dev/null
cd "$dest"
remote=$PWD/.jj/fixture-remote.git
git init --quiet --bare --initial-branch=main "$remote"
j git remote add origin "$remote"

png_a=iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAIAAABt+uBvAAAAtklEQVR42u3bQQ0AMAgAsQlD2IQhcCbutZScAfolnHM2aaJu1EYdQIAAAQIECBAgQIAAAQIECBAgQIAAAQIECBAgQH8AzSZN1I3aKECAAAECBAgQIECAAAECBAgQIECAAAECBAgQIECfAN1NmqgbtVGAAAECBAgQIECAAAECBAgQIECAAAECBAgQIECAPgGKXs+qU121WjWAAAECBAgQIECAAAECBAgQIECAAAECBAgQIECAvgB6Heei7w0TB44AAAAASUVORK5CYII=
png_b=iVBORw0KGgoAAAANSUhEUgAAAGAAAABgCAIAAABt+uBvAAAAsUlEQVR42u3QQQ3AMAwAsQILsAILiMEaiXtVlozA5zunMZEb2cYRJEiQIEGCBAkSJEiQIEGCBAkSJEiQIEGCBAkS9EjQNCZyI9sQJEiQIEGCBAkSJEiQIEGCBAkSJEiQIEGCBAkS9ErQbUzkRrYhSJAgQYIECRIkSJAgQYIECRIkSJAgQYIECRIk6JWgbUzkRrYhSJAgQYIECRIkSJAgQYIECRIkSJAgQYIECRIk6ImgHy4bcxzc2u5xAAAAAElFTkSuQmCC
png_hero=iVBORw0KGgoAAAANSUhEUgAAAUAAAAC0CAIAAABqhmJGAAADtUlEQVR42u3TMXHFQBTAQCNxHRAHIrWRPCRXfxAGkdqIMpOYg5rViMIexzHJZ/SKvqInekff0U/0ATDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAAD/A/4a5LP6BV9RU/0jr6jn2iAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGOAX8Pckn9Er+oqe6B19Rz/RAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAL+CZ5DN6RV/RVTv6jn6iAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAX8CfST6jV/QVPdE7+o5+ogEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggAEGGGCAAQYYYIABBhhggF/AP5N8Rq/oK3qid/Qd/UQDDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAADDDDAAAMMMMAAAwwwwAD//QvLGbW0v3gqHgAAAABJRU5ErkJggg==

# Initial import ------------------------------------------------------------
ada
j describe -m 'Initial import of the render toolkit'
mkdir -p src/renderer src/generated assets vendor i18n data docs scripts
cat >src/main.rs <<'RS'
use std::env;
use std::process::ExitCode;

mod config;
mod old_name;

/// Entry point: parse arguments and hand off to the renderer.
fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: render <scene>");
        return ExitCode::FAILURE;
    };
    let settings = config::load();
    match old_name::render(path, &settings) {
        Ok(frames) => {
            println!("rendered {frames} frames");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("render failed: {error}");
            ExitCode::FAILURE
        }
    }
}
RS
cat >src/config.rs <<'RS'
pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub samples: u32,
}

pub fn load() -> Settings {
    Settings {
        width: 1920,
        height: 1080,
        samples: 4,
    }
}
RS
{
  cat <<'RS'
//! The frame loop. Moved around a lot, so it makes a good rename test.
use crate::config::Settings;

pub fn render(path: &str, settings: &Settings) -> Result<u32, String> {
    let mut frames = 0;
RS
  for i in $(seq 1 60); do
    printf '    // step %02d: resolve bindings for pass %d\n    frames += prepare_pass(path, settings, %d)?;\n' "$i" "$i" "$i"
  done
  cat <<'RS'
    Ok(frames)
}

fn prepare_pass(path: &str, settings: &Settings, pass: u32) -> Result<u32, String> {
    if path.is_empty() {
        return Err(format!("empty scene path at pass {pass}"));
    }
    Ok(settings.samples)
}
RS
} >src/old_name.rs
cat >src/removed.rs <<'RS'
//! Legacy software rasterizer. Nothing calls it anymore.
pub fn rasterize(width: u32, height: u32) -> Vec<u8> {
    vec![0; (width * height * 4) as usize]
}
RS
cat >README.md <<'MD'
# Render toolkit

A small scene renderer. Build with `make`, run with `render <scene>`.

## Layout

- `src/` the renderer
- `assets/` logos and hero images
- `i18n/` translated strings
MD
printf 'CC\t:= cc\nCFLAGS\t:= -O2 -Wall\n\n.PHONY: all clean\n\nall: render\n\nrender: main.o\n\t$(CC) $(CFLAGS) -o $@ $<\n\nclean:\n\trm -f render *.o\n' >Makefile
printf 'package main\n\nimport "fmt"\n\nfunc main() {\n\tfor i := 0; i < 3; i++ {\n\t\tif i%%2 == 0 {\n\t\t\tfmt.Println("tab\\tseparated", i)\n\t\t}\n\t}\n}\n' >scripts/tool.go
printf '#!/bin/sh\nexec ./render "$@"\n' >scripts/run.sh
cat >i18n/ja.txt <<'TXT'
# 翻訳ファイル
greeting = こんにちは、世界
farewell = さようなら
render.start = レンダリングを開始します
render.done = 完了しました (混在 mixed 文字列 with ASCII)
error.missing = ファイルが見つかりません
TXT
cat >i18n/zh.txt <<'TXT'
greeting = 你好，世界
farewell = 再见
render.start = 开始渲染
emoji = 完成 🎉 成功 ✅
TXT
cat >assets/logo.svg <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <rect width="128" height="128" rx="24" fill="#3b6fd4"/>
  <circle cx="64" cy="64" r="36" fill="#ffffff"/>
</svg>
SVG
base64 -d <<<"$png_a" >assets/logo.png
head -c 4096 /dev/zero | tr '\0' '\1' >vendor/blob.bin
printf '\0\1\2\3binary-v1\0' >>vendor/blob.bin
awk 'BEGIN { s = "function a(){return["; for (i = 0; i < 900; i++) s = s "\"item-" i "\","; print s "]}"; print "const version = \"1\";" }' >src/generated/minified.js
awk 'BEGIN { for (i = 1; i <= 20000; i++) printf "%06d  row value %d checksum %x\n", i, i * 7, i * 2654435761 % 65536 }' >data/large.txt
cat >docs/guide.md <<'MD'
# Guide

Rendering runs in passes. Each pass resolves bindings, then draws.
MD
j bookmark create v0-import -r @

# marks: name -> change id, resolved to commit ids at the end.
: >.jj/fixture-marks
mark() { "$JJ" --quiet log --ignore-working-copy --no-graph -r "${2:-@}" -T "\"$1 \" ++ change_id ++ \"\n\"" >>.jj/fixture-marks; }

# Trunk: enough linear history that the graph scrolls --------------------------
bob
start @ -- 'docs: describe the scene file format'
printf '\n## Scene files\n\nA scene is a TOML file listing cameras and lights.\n' >>docs/guide.md
for i in $(seq 1 40); do
  case $((i % 3)) in 0) ada ;; 1) bob ;; 2) mei ;; esac
  start @ -- "$(printf 'chore(changelog): record release candidate %02d' "$i")"
  printf 'rc%02d: fixes and small improvements\n' "$i" >>CHANGELOG.txt
done
snap
base_old=$("$JJ" log --ignore-working-copy --no-graph -r 'description(substring:"candidate 28")' -T 'change_id')
base_mid=$("$JJ" log --ignore-working-copy --no-graph -r 'description(substring:"candidate 34")' -T 'change_id')
ada
j bookmark create main -r @
j bookmark create feature/deleted-locally -r "$base_old"
j bookmark create experiment/remote-only -r "$base_mid"
j bookmark create release/conflicted -r "$base_old"
git tag v0.1.0 "$("$JJ" log --ignore-working-copy --no-graph -r "$base_old" -T commit_id)"
j git import
push_new main feature/deleted-locally experiment/remote-only release/conflicted

# feature/render-pipeline: the kitchen-sink diff ----------------------------------
ada
start "$base_mid" -- 'feat(render): rework the pipeline and move the frame loop

Moves the frame loop out of `old_name.rs`, drops the software rasterizer, and
refreshes the logo. Touches one of everything a diff can show: a rename, a
delete, a binary, images, an svg, very long lines, tabs, CJK, a mode change, an
empty file, a deeply nested path and a whole-file rewrite.

Refs: https://example.com/issues/1234'
mark kitchen-sink
mv src/old_name.rs src/renderer/frame_loop.rs
sed -i.bak -e 's/step 12: resolve bindings/step 12: resolve and validate bindings/' \
  -e 's/empty scene path/empty or missing scene path/' src/renderer/frame_loop.rs
rm -f src/renderer/frame_loop.rs.bak
rm src/removed.rs
sed -i.bak -e 's/mod old_name;/mod renderer;/' -e 's/old_name::render/renderer::frame_loop::render/' \
  -e 's/render failed:/renderer failed:/' src/main.rs
rm -f src/main.rs.bak
printf 'pub mod frame_loop;\n' >src/renderer/mod.rs
base64 -d <<<"$png_b" >assets/logo.png
base64 -d <<<"$png_hero" >assets/hero.png
cat >assets/logo.svg <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <rect width="128" height="128" rx="24" fill="#d4693b"/>
  <circle cx="64" cy="64" r="36" fill="#ffe9c7"/>
  <path d="M44 64h40M64 44v40" stroke="#d4693b" stroke-width="8"/>
</svg>
SVG
cat >assets/mark.svg <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><circle cx="32" cy="32" r="28" fill="#2f9e6e"/></svg>
SVG
printf '\0\1\2\3binary-v2 with more payload\0' >>vendor/blob.bin
head -c 2048 /dev/zero >vendor/new-blob.dat
awk 'BEGIN { s = "function a(){return["; for (i = 0; i < 900; i++) { if (i == 450) s = s "\"item-changed\","; else s = s "\"item-" i "\"," } print s "]}"; print "const version = \"2\";"; t = "// "; for (i = 0; i < 400; i++) t = t "long-prose-word-" i " "; print t }' >src/generated/minified.js
printf 'CC\t:= clang\nCFLAGS\t:= -O3 -Wall\t-Wextra\n\n.PHONY: all clean test\n\nall: render\n\nrender: main.o frame_loop.o\n\t$(CC) $(CFLAGS) -o $@ $^\n\ntest:\n\t\t./render --check\t# doubly indented\n\nclean:\n\trm -f render *.o\n' >Makefile
sed -i.bak -e 's/i%2 == 0/i%3 == 0/' -e 's/tab\\tseparated/tab\\tseparated\\tagain/' scripts/tool.go
rm -f scripts/tool.go.bak
cat >i18n/ja.txt <<'TXT'
# 翻訳ファイル (更新済み)
greeting = こんにちは、素晴らしい世界
farewell = さようなら
render.start = レンダリングを開始します
render.done = すべて完了しました (混在 mixed 文字列 with ASCII と全角ＡＢＣ)
error.missing = ファイルが見つかりません: {path}
error.long = これは非常に長い行です。折り返しと列の境界を確認するために、同じ内容を何度も繰り返しています。これは非常に長い行です。折り返しと列の境界を確認するために、同じ内容を何度も繰り返しています。これは非常に長い行です。折り返しと列の境界を確認するために、同じ内容を何度も繰り返しています。
TXT
printf 'emoji = 全部完成 🎉🎉 成功 ✅\n' >>i18n/zh.txt
chmod +x scripts/run.sh
: >docs/empty-file.txt
deep=src/this/path/is/deliberately/nested/very/deeply/so/that/the/file/row/has/to/ellipsize/in/the/changes/list/and/the/diff/header
mkdir -p "$deep"
printf 'pub const DEEP: u32 = 7;\n' >"$deep/a_module_with_a_rather_long_file_name_as_well.rs"
snap

bob
start @ -- 'perf(render): cache resolved bindings between passes'
mark cache-bindings
sed -i.bak 's/Ok(settings.samples)/Ok(settings.samples * 2)/' src/renderer/frame_loop.rs
rm -f src/renderer/frame_loop.rs.bak
printf '\n## Caching\n\nBindings are cached between passes.\n' >>docs/guide.md
snap
ada
j bookmark create feature/render-pipeline -r @
push_new feature/render-pipeline

# One local commit past the remote, so the bookmark reads as ahead.
start @ -- 'fix(render): clamp the sample count'
sed -i.bak 's/samples: 4,/samples: 4.min(16),/' src/config.rs
rm -f src/config.rs.bak
snap
j bookmark move feature/render-pipeline --to @
pipeline_tip=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)

# feature/auth: pushed, then rewritten, so local and remote diverge ----------------
mei
start "$base_mid" -- 'feat(auth): add token refresh'
mark auth-refresh
mkdir -p src/auth
cat >src/auth/token.rs <<'RS'
pub struct Token {
    pub value: String,
    pub expires_at: u64,
}

impl Token {
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}
RS
snap
start @ -- 'feat(auth): retry once on an expired token'
printf '\npub fn retry_once() -> bool {\n    true\n}\n' >>src/auth/token.rs
snap
j bookmark create feature/auth -r @
push_new feature/auth
j describe -m 'feat(auth): retry with backoff on an expired token'
printf 'pub const BACKOFF_MS: u64 = 250;\n' >>src/auth/token.rs
snap
auth_tip=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)

# fix/parser-crash: local only, merged with auth -----------------------------------
bob
start "$base_mid" -- 'fix(parser): do not crash on an empty scene'
mkdir -p src/parser
printf 'pub fn parse(input: &str) -> Option<&str> {\n    input.lines().next()\n}\n' >src/parser/mod.rs
snap
j bookmark create fix/parser-crash -r @
parser_tip=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)
ada
start "$auth_tip" "$parser_tip" -- 'Merge fix/parser-crash into feature/auth'
mark merge-commit
printf 'merged parser and auth\n' >docs/merge-notes.txt
snap

# Metadata oddities ---------------------------------------------------------------
long
start "$base_mid" -- 'refactor: this description has a deliberately enormous first line so that every place showing a revision subject has to ellipsize it instead of running under a scrollbar or into the next column, and it keeps going for a while longer just to be sure that even a very wide window cannot fit all of it on one row of the graph

Second paragraph with ordinary length.

Third paragraph, a list:
- first thing
- second thing, 複数行の説明 with CJK
'
mark long-description
printf 'long description touched this\n' >docs/long.txt
snap
j bookmark create feature/this-is-an-extremely-long-bookmark-name-that-has-to-ellipsize-in-every-column-it-appears-in -r @
j new "$base_mid" -m 'chore: an empty revision with a description'
mark empty-revision
mei
start "$base_mid" -- ''
mark no-description
printf 'このリビジョンには説明がありません\n' >docs/no-description.txt
snap

# Many files: lists must scroll and keep their gutter -------------------------------
bob
start "$base_mid" -- 'chore(modules): generate 240 module stubs'
mark many-files
mkdir -p src/modules
for i in $(seq -w 1 240); do
  printf 'pub fn module_%s() -> u32 {\n    %d\n}\n' "$i" "$((10#$i))" >"src/modules/mod_$i.rs"
done
snap

# Whole-file rewrite of 20k lines: the scroll and redraw target ----------------------
ada
start "$base_mid" -- 'data: regenerate the large table'
mark large-diff
awk 'BEGIN { for (i = 1; i <= 20000; i++) printf "%06d  row value %d checksum %x rev2\n", i, i * 7, i * 2654435761 % 65536 }' >data/large.txt
snap
j bookmark create data/regenerate -r @

# Conflicts -------------------------------------------------------------------------
bob
start "$base_old" -- 'settings: raise the default sample count'
sed -i.bak 's/samples: 4,/samples: 8,/' src/config.rs
rm -f src/config.rs.bak
sed -i.bak 's/usage: render <scene>/usage: render [options] <scene>/' src/main.rs
rm -f src/main.rs.bak
printf '//\n// Kept for one more release.\n' >>src/removed.rs
snap
settings_a=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)
mei
start "$base_old" -- 'settings: lower the default sample count for laptops'
mark conflicted
sed -i.bak -e 's/samples: 4,/samples: 2,/' -e 's/width: 1920,/width: 1280,/' src/config.rs
rm -f src/config.rs.bak
sed -i.bak 's/usage: render <scene>/usage: render <scene> [--fast]/' src/main.rs
rm -f src/main.rs.bak
rm src/removed.rs
snap
settings_b=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)
start @ -- 'settings: document the sample count'
mark conflicted-child
printf '\nThe default sample count depends on the machine.\n' >>docs/guide.md
snap
j bookmark create conflict/settings -r @
j rebase -s "$settings_b" -d "$settings_a"

ada
sides=()
for side in one two three; do
  start "$base_old" -- "guide: rewrite the intro, take $side"
  sed -i.bak "s/Rendering runs in passes./Rendering runs in passes, take $side./" docs/guide.md
  rm -f docs/guide.md.bak
  snap
  sides+=("$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)")
done
start "${sides[@]}" -- 'Merge the three guide rewrites'
mark conflicted-octopus
snap
j bookmark create conflict/octopus -r @

# Divergent change: same change id, two commits -------------------------------------
bob
start "$base_mid" -- 'experiment: try a divergent change'
printf 'divergent\n' >docs/divergent.txt
snap
divergent=$("$JJ" log --ignore-working-copy --no-graph -r @ -T change_id)
j new "$base_mid" -m 'scratch: moved away from the divergent change'
op=$("$JJ" op log --ignore-working-copy --no-graph -n 1 -T 'id')
j --ignore-working-copy describe -r "$divergent" -m 'experiment: divergent change, take A'
j --ignore-working-copy --at-op "$op" describe -r "$divergent" -m 'experiment: divergent change, take B'
j abandon @
j bookmark create divergent/take-a -r 'description(substring:"divergent change, take A")'
j bookmark create divergent/take-b -r 'description(substring:"divergent change, take B")'

# Bookmark states -------------------------------------------------------------------
ada
j bookmark delete feature/deleted-locally
j bookmark forget experiment/remote-only
# The remote side has to be a commit the remote already holds and that isn't an
# ancestor of the local side, or the fetch resolves instead of conflicting.
other=$("$JJ" log --ignore-working-copy --no-graph -r feature/auth@origin -T commit_id)
git --git-dir="$remote" update-ref refs/heads/release/conflicted "$other"
j bookmark move release/conflicted --to "$pipeline_tip" --allow-backwards
j git fetch

# A branch that conflicts with trunk: rebasing it onto main forecasts a conflict.
bob
start "$base_mid" -- 'docs(changelog): note the breaking change'
mark trunk-conflict
printf 'rc35: this line collides with what trunk wrote here\n' >>CHANGELOG.txt
snap
j bookmark create fix/changelog-note -r @

# Working copy with changes on the pipeline tip -----------------------------------
ada
start "$pipeline_tip" -- ''
mark working-copy
sed -i.bak -e 's/render failed:/render pass failed:/' -e 's/usage: render <scene>/usage: render <scene> [--samples N]/' \
  -e 's/rendered {frames} frames/rendered {frames} frames in total/' src/main.rs
rm -f src/main.rs.bak
sed -i.bak -e 's/step 03: resolve/step 03: look up and resolve/' -e 's/step 30: resolve/step 30: look up and resolve/' \
  -e '/step 45:/d' src/renderer/frame_loop.rs
rm -f src/renderer/frame_loop.rs.bak
cat >docs/release-notes.md <<'MD'
# Release notes

- Pipeline rework
- 日本語の翻訳を更新
MD
rm docs/empty-file.txt
mv scripts/run.sh scripts/start.sh
base64 -d <<<"$png_hero" >assets/working-copy.png
printf 'uncommitted\tchange\twith tabs\n' >>scripts/tool.go
snap

# Manifest --------------------------------------------------------------------------
"$JJ" --quiet log --ignore-working-copy --no-graph -r 'main | feature/render-pipeline | feature/auth | fix/parser-crash | fix/changelog-note | data/regenerate | conflict/settings | conflict/octopus | divergent/take-a | divergent/take-b' \
  -T 'bookmarks.map(|b| b.name()).join(",") ++ " " ++ change_id ++ " " ++ commit_id ++ "\n"' >.jj/fixture-manifest
while read -r name change; do
  commit=$("$JJ" log --ignore-working-copy --no-graph -r "$change" -T commit_id)
  printf '%s %s %s\n' "$name" "$change" "$commit" >>.jj/fixture-manifest
done <.jj/fixture-marks
rm .jj/fixture-marks
