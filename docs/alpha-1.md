# Alpha 1 contract: repository editing loop

Contract status: Accepted (2026-08-23)

Gate statusは保存せず、A6のcandidate/promotion規則とevidence checkから導出する。
promotion `P`がevidence-onlyのdirect childである場合に限り、親candidate `C`に対する
Single decision ruleの結果を`P`が継承する。

## Outcome

Linux上で`zec DIRECTORY`を起点に、対象fileを事前にCLI引数へ列挙せず、
repository内のfile discovery、project-wide search、複数fileの編集、新規file作成、
保存、終了、再起動、再openまでを完了できる状態をAlpha 1とする。

機能数や人間のdogfood時間は判定に使わない。固定fixtureに対するactual-binary PTY試験、
exact filesystem manifest、latency/RSS assertion、GitHub-hosted CIのexit statusだけで判定する。

## Single decision rule

clean checkoutで次を順に実行し、すべてが1回でexit 0になったcandidate commitだけが
promotion対象になる。

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
cargo build --locked --release \
  --bin zec --bin alpha_1_acceptance --bin alpha_1_bench
cargo test --locked --bin zec -- --test-threads=1
cargo test --locked --test pty_acceptance -- --test-threads=1
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/alpha_1_acceptance --zec ./target/release/zec \
  --repo . --assert --report target/alpha-1/acceptance.json
./target/release/alpha_1_acceptance \
  --verify-report target/alpha-1/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/alpha_1_bench --zec ./target/release/zec \
  --assert --report target/alpha-1/benchmark.json
./target/release/alpha_1_bench \
  --verify-report target/alpha-1/benchmark.json
```

同じcommandを`ubuntu-24.04`のGitHub-hosted runnerで実行する`Alpha 1 gate` jobも
`success`でなければならない。job timeoutは90分、retryは0回とする。command timeout、
test retry、手動確認、既知flakeの再実行はPassに数えない。卒業記録へlinkするrunは
cancelされていない単一attemptでなければならず、結果が`success`以外ならFailとする。
修正commitには新しいrunを使う。

## Fixed test environment

- runner class: GitHub-hosted Ubuntu 24.04 x86_64。CPU型番、core数、RAM、kernel、
  runner image versionをreportへ記録する。hardware差や負荷はretry理由にしない
- locale / terminal: `C.UTF-8`、`TERM=xterm-256color`、120x40 cells
- binary: `--release`でbuildした同じcommitの`zec`
- clock: `CLOCK_MONOTONIC`相当。時間は整数microsecondsで保持し、sample数とwarm-up数は
  A4の各metricで固定する
- PTY parser: `vt100 0.16.2`を`Cargo.lock`で固定する
- repository fixture: seed `0x5a45435f414c5048`から生成する。rootと`.git`内部を除き
  10,000 entries（9,900 regular files、96 directories、4 symlinks）、UTF-8 text payload
  合計104,857,600 bytes、100,000 logical linesかつ99,999 LFのfileを含める
- fixtureにはspace/Unicode path、`.gitignore`対象、NULを含むbinary、root外control file、
  symlink alias、UTF-8 BOM、CRLF、final newlineなしの編集対象を含める
- Alpha 1で編集対象にするregular fileは10 MiB以下、1 display lineは64 KiB以下とする

fixtureのnormative input、query、expected resultは`tests/alpha_1/spec-v1.json`へ置く。
manifestはrelative pathのUTF-8 byte順JSONLとし、各recordを`path`、`kind`、4桁octal
`mode`、`size`、lowercase `content_sha256`、`symlink_target`へ固定する。
mtimeとinodeは含めず、各recordと末尾をLFで終える。generator source SHA-256、
spec SHA-256、操作前とexpected操作後のmanifest SHA-256をreportへ残す。

## Normative oracles and timing

- contractのoracleはversion管理された`spec-v1.json`、expected manifest、VT predicateである。
  Zed API名への言及とImplementation orderはnon-normativeな実装方針とする
- harnessは各read完了時にVT parserを更新してgenerationを1増やす。frame到達時刻は、
  指定predicateへ初めて一致したgenerationのread完了時刻とする
- startupの開始はchild spawn call直前、それ以外の開始はPTY writerが操作の最終byteを
  flushした直後とする
- Ready predicateはalternate screen、120x40、fixture root label、期待本文sentinel、
  body内のvisible cursorが同じVT generationに存在することとする
- p95は昇順raw samples `x`に対するnearest-rank
  `x[ceil(0.95 * N) - 1]`、maxは`x[N - 1]`とする
- 1つのscreen predicate待ちは15秒、child cleanupは5秒、1 PTY scenarioは120秒で
  timeoutとし、timeoutは必ずFailとする
- acceptance reportは`contract_version=1`、必須case IDの重複・欠落なし、
  `failed=0`、runner/locale/terminal/binary/parser条件の完全一致を満たさなければ
  verify commandがnon-zeroで終了する
- required binary/report/specが欠けるinitial stateはFailである

## Gates

### A0. PoC regression

先頭2つのtest commandで既存PoC graduationのunit/headless 93件とactual-binary PTY 1件を
すべて通す。`tests/alpha_1/poc-test-ids-v1.txt`へbaseline 94 test IDを固定し、
acceptance verifierが現在の`cargo test -- --list`に全IDが含まれることを確認する。
追加testは許可するが、既存caseの削除・ignore・filterによる欠落はFailとする。
### A1. Directory root and file identity

`alpha_1_acceptance`のheadless caseで次をassertする。

- cwdと`DIRECTORY`、`.`、`..`、absolute path、symlink aliasの入力値を
  `spec-v1.json`へliteralで列挙する
- directoryはfileとしてopenされず、repository root IDは1個、root配下のworktree root IDも
  1個であり、fileごとの重複worktreeを作らない
- specに列挙した全aliasの`buffer_id`と`tab_id`がそれぞれ完全一致する
- `.git`、`target`、ignore対象には専用sentinelを置き、quick-open/project searchの
  expected result JSONがいずれも0件である
- root外fileのtracing FSをresetしてopenし、そのfile自身へのopen/statだけを許可する。
  outside parentとsiblingsへの`read_dir` call countは0である
- startupには正常fileとself-referential symlinkを同時に渡す。`ELOOP`を表示した後もReadyへ到達し、
  正常tabの固定tokenを編集・保存してexit code 0になる

### A2. Quick open and project search

actual-binary PTY caseは`Ctrl-P = 0x10`をquick open、`Alt-F = ESC f`を
project-wide literal searchとして固定する。

- quick-open query `日本 語.rs`のselected resultとEnter後のpathを
  `spec-v1.json`のexpected JSONと完全一致させる。同じaliasを再度openしてもtab countは増えない
- searchはcase-sensitive、Unicode normalizationなし、result limit 100とする。lineはLF/CRLFで
  区切る1-based logical line、columnはBOMを除いた1-based Unicode scalar indexとする
- resultのpath、line、column、preview、順序をqueryごとのexpected JSONと完全一致させ、
  Enter後のcaretをexpected match startへ置く。restart前cursorの復元は要求しない
- ignored、binary、outside対象の各fileに`ALPHA1_EXCLUDED_SENTINEL`を置き、同じsentinelを持つ
  in-scope control file 1件だけが返ることをassertする
- search中のEscではprompt消滅、別queryでは新queryとそのexpected results、`Ctrl-Q`では
  child exitをそれぞれ15秒以内に観測し、その後に古いresultを描画・適用しない

controlled headless caseはproductionと同じcommand/reducerへcompletion順だけを差し替える
deterministic schedulerを使う。query Aを保留、query Bを完了、最後にAを完了させ、
publish logがBのexpected result 1回だけで、最終stateもBのままであることをassertする。
actual-binary PTY caseはshortcut byte、prompt、result選択、cancel、openの配線をassertする。

file list、search range、open結果をZedのworktree/project/buffer APIから取得し、zecが本文、
selection、undo、dirty stateを複製しない方針はnon-normative implementation noteである。
Pass/Failのnormative oracleは上記expected JSONとstate traceだけとする。

### A3. Exact multi-file workflow

`alpha_1_acceptance`は実バイナリをcontrolling PTYへ起動し、キー入力だけで次を実行する。

1. directoryから起動し、quick-openでUTF-8 BOM file Aを開いてselection置換する。
2. project searchからCRLF file Bを開いて別tokenを編集する。
3. quick-openでfinal newlineなしのfile Cを開いて行末を編集する。
4. scratch tabを作り、Unicode path/textのfile Dとしてroot配下へSave Asする。
5. A/B/C/Dを保存し、tab statusのdirty/conflict/deleted markerが0であることを確認する。
6. 終了後の全manifestをexpected manifestと完全一致させ、編集対象外のpath、mode、bytesが
   1つも変わっていないことを確認する。
7. freshな第2processで同じdirectoryを起動し、PTY操作でA/B/C/Dを再度openする。
   quick-open/searchから開いたcaretが各expected match startで、textがexpected bytesであることを
   確認する。restart前のcursor/session復元は要求しない。
8. exit code 0、終了後の`tcgetattr`と起動前baselineの完全一致、alternate screen、mouse tracking、
   bracketed paste、cursor visibility、application cursor/keypad modeのbaseline復帰を確認する。

`A3_WORKFLOW_01`から`A3_WORKFLOW_20`まで、fresh fixture、fresh config directory、
fresh processで逐次実行し、retryは0回とする。編集/paste payloadには連番付きunique tokenを使い、
送信token列、画面適用token列、expected file token列を完全一致させる。

### A4. Responsiveness and resource envelope

`alpha_1_bench --assert`はspecに固定したinputとVT predicateで次をassertする。

- directory起動: spawn直前からReady generationまで。2 warm-up後20 fresh launchesの
  p95が3,000 ms以下
- quick-open: index-ready状態で、PTY flushからexpected selected result generationまで。
  10 warm-up後100 spec queriesのp95 150 ms以下、max 500 ms以下
- project search: 各sampleをfresh processのindex-ready状態にし、過去に実行していない
  `ALPHA1_BENCH_SEARCH`を送る。expected hit数1,000のcomplete generationまでを
  2 warm-up後10 samples測定し、p95 5,000 ms以下。total hit countは1,000、画面へ出すlistは
  specに固定した先頭100件とする
- in-flight search: query replacementは新queryとexpected results、cancelはprompt消滅、
  quitはchild回収を終了eventとする。各20 attemptsのmax 250 ms以下
- editing: 100,000行fileへ連番tokenをinsertし、そのtokenがexpected cellへ現れるgenerationまで。
  10 warm-up後500 editsのp95 100 ms以下、max 500 ms以下
- save: 5 MiB fileの固定offsetを各sample直前に1 byte変更してdirtyにし、`Ctrl-S` flushから
  dirty marker消滅とexpected disk bytesの両方まで。2 warm-up後10 samplesのmax 2,000 ms以下
- zec main processの`/proc/PID/status` `VmHWM`を1,024倍してbytesへ変換した値が
  1,073,741,824以下で、benchmark中のdescendant process countが0
- reportの`sent_input_ids`、`applied_input_ids`、expected ID列が完全一致し、
  `dropped_count=0`かつreorderなし

benchmark reportはschema version、environment、各raw sample、warm-up/sample count、
p50/p95/max、VmHWM、input ID列をJSONへ保存する。verify modeはraw samplesから統計値を
再計算する。単一の巨大lineと極端なmatch数はfixtureへ含めず、既知制約として維持する。

### A5. Failure and terminal lifecycle

各caseはfresh fixture、fresh config、fresh controlling PTY、fresh foreground process groupで行う。

- open failureはself-referential symlinkによる`ELOOP`、save failureはregular fileを親にした
  child pathへのSave Asによる`ENOTDIR`をactual binaryへ発生させる
- search failureはproduction reducerへcontrolled providerから`EIO`を返す。error表示後も
  既存tab count、本文、dirty stateが直前traceと一致し、control tabの編集・保存を続行できる
- `SIGINT`、`SIGQUIT`、`SIGTERM`、`SIGHUP`はReady後にforeground process groupへ
  `killpg`で送る。5秒以内にnormal exit code 0で回収し、`tcgetattr`完全一致とalternate screen、
  mouse tracking、bracketed paste、cursor visibility、application cursor/keypad modeの
  baseline復帰をassertする
- `SIGTSTP`は`killpg`後5秒以内に`waitpid(WUNTRACED)`でstoppedを確認し、その時点で
  terminal baselineへ復帰していることをassertする。`SIGCONT`後15秒以内にReadyへ戻り、
  固定tokenの編集・保存と`Ctrl-Q`によるexit code 0まで完走する
- 各childは5秒以内に`try_wait`で回収し、reader threadも5秒以内にjoinする。終了後の
  process groupにdescendant PIDがなく、harnessの`/proc/self/fd` countが開始前と一致する

open、search、save failureとINT、QUIT、TERM、HUP、TSTP/CONTの各scenarioを20回ずつ実行する。
reportの必須IDは`A1_ROOT_IDENTITY`、`A1_OUTSIDE_TRACE`、`A1_PARTIAL_STARTUP`、
`A2_QUICK_OPEN`、`A2_PROJECT_SEARCH`、`A2_STALE_RESULT`、
`A3_WORKFLOW_01..20`、`A5_{OPEN,SEARCH,SAVE}_01..20`、
`A5_{INT,QUIT,TERM,HUP,TSTP_CONT}_01..20`の展開後186件とする。
全IDをちょうど1回実行し、1件でも欠落、重複、失敗したらreport verificationをFailにする。

### A6. CI evidence

Pass判定はcandidateとpromotionの2 commitで行い、self-referenceを作らない。

1. candidate commit `C`でpush eventの`Alpha 1 gate` run `R`をretry 0で完了させる。
   `R`はacceptance/benchmark reportをartifactとしてuploadし、job conclusionを
   `success`にする。
2. `C`のdirect childとなるpromotion commit `P`は
   `docs/alpha-1-evidence.json`だけを追加する。そこへ`C`のfull SHA、`R`のrun/job URLとID、
   run attempt、generator/spec/before/after manifest SHA-256、report SHA-256、既存test count、
   acceptance case count 186を記録する。
3. `P`の`Alpha 1 evidence` jobはGitHub APIから、`R.head_sha == C`、
   `R.event == push`、`R.run_attempt == 1`、gate job conclusionが`success`、artifact digest一致を
   確認する。同じ`workflow_id`、`event == push`、`head_sha == C`を満たすrun IDの集合が
   `[R.id]`だけであることも確認し、別run IDによるやり直しを拒否する。
4. 同じevidence jobは`P^ == C`、`C..P`の変更pathがevidence JSONだけ、
   acceptanceが186/186、benchmarkの全assertがtrueであることを確認する。さらに自身の
   `GITHUB_RUN_ATTEMPT == 1`と、同じ`workflow_id`、`event == push`、`head_sha == P`の
   run ID集合が自身のIDだけであることを確認する。

default branch上の`Alpha 1 evidence` jobが`success`であることだけをcanonicalなPassとする。
文書status、issue checklist、実地dogfood報告、workflow全体の実行中URLは判定に使わない。

## Explicit exclusions from the gate

- LSP、completion、diagnostics、go-to-definition、rename
- file tree sidebar、Git UI、integrated terminal/task runner
- plugin、Zed settings/keymap互換、session restore、crash recovery
- project-wide replace、regex/case/word search option
- mouse drag/double/triple selection、terminal-aware soft wrap
- macOS、Windows、remote filesystem、複数root workspace
- package配布、外部ユーザー向けrelease
- power lossに耐えるatomic/durable save
- 10 MiB超のfile、64 KiB超の単一display line、極端な全件match

この一覧はAlpha 1で測定しないscopeを定めるもので、追加の主観判定ではない。

固定Zed revisionの非atomicな保存経路は既知制約として残る。Alpha 1におけるsave成功は
Zedのsave taskがwrite/closeを成功させ、dirty=falseとなり、期待disk bytesを読める地点までとする。
`fsync`、atomic rename、power-loss durabilityは要求しない。fixtureとdogfood対象はcleanな
Git管理下のsource treeに限定する。

## Non-normative implementation notes

zec独自のtext modelや`std::fs`保存でZedを迂回しない。A0のPoC regression gateは既存の
編集権限境界を維持するが、この実装note自体は人手のPass条件にしない。

1. feature実装前にfixture generator、expected manifest、failing `alpha_1_acceptance`を追加する。
2. directory root/file identityを1つにする。
3. quick-openを通し、その同じroot modelでproject searchを通す。
4. exact multi-file/reopen scenarioをgreenにする。
5. async cancellation、performance、signal/job-controlを閉じる。
6. hosted `Alpha 1 gate`を1回でgreenにし、promotion evidence jobをgreenにする。

実装順や「それ以外の機能を先に作らない」という運用方針はPass/Failには影響しない。
