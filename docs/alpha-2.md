# Alpha 2 contract: Project-backed language editing loop

Contract status: Accepted (2026-08-26)

Gate statusは保存しない。candidate commitとretryなしのhosted gate artifact、および
evidence-only direct childの検証結果から導出する。Alpha 1のcanonical evidence規則を継承し、
Alpha 2 promotionはAlpha 1を再検証する。

## Implementation checkpoint (not gate status)

2026-08-26時点のworking candidateは、単一Zed Project、native LanguageRegistry/LspStore、
command palette、completion、hover、project diagnostics、definition/type-definition/references、
project symbols、back/forward history、editable MultiBuffer、prepareRename/rename、code actions、
document/range formattingをproduction経路へ接続している。settings/keymapのprecedenceとlive reload、
format-on-save、WorkspaceEditのfile operation、worktree trust、path/symlink/special-file境界、
LSP failure/restart/process cleanup、大容量payloadの上限もactual `zec` binaryで検査する。

決定的fixture LSPは通常の`rust-analyzer` PATH discoveryで起動する。integration testに加え、
20 fresh processのlanguage/PTY workflow、15 failure scenarioを各20 fresh processで実行する341 caseの
acceptance harnessと、raw sample・correlation ID・VmHWMを保存するbenchmark harnessが存在する。
短縮sampleでの開発用runは完走しているが、これはB0〜B9のcanonical Pass宣言ではない。
retryなしのhosted candidate artifactとevidence-only promotionが成功するまで台帳は`candidate`に留め、
下記single decision ruleを短縮しない。

## Outcome

Linux上のfreshなRust repositoryを`zec DIRECTORY`で開き、Zedのlocal `Project`、settings、
LanguageRegistry、LspStoreを通して、command discovery、completion、diagnostics、hover、
definition/reference navigation、code action、rename、format、複数file保存をconsoleだけで完了できる
状態をAlpha 2とする。

外部language serverはnetworkから取得せず、gateがbuildした決定的なfixture serverをproductionの
PATH discoveryで選ぶ。test-only providerをzecへinjectせず、実行中binaryは通常のProject/LSP経路を使う。

## Single decision rule

candidateは次のcommandをclean checkoutで順に実行し、すべてが1回でexit 0にならなければならない。
具体的なbinary名と引数はfixture/harness実装と同じcommitで固定し、この節へ追加するまでは
Alpha 2をpromotionしてはならない。

```sh
export LC_ALL=C.UTF-8 LANG=C.UTF-8 TERM=xterm-256color
rustc --edition=2024 src/bin/alpha_1_fixture.rs -o /tmp/zec-alpha-1-fixture-verifier
/tmp/zec-alpha-1-fixture-verifier verify-oracles --repo .
cargo test --locked --release --features alpha-1-linux \
  --bin zec -- --test-threads=1
cargo test --locked --release --features alpha-1-linux \
  --test parity_contract --test pty_acceptance \
  --test alpha_2_lsp --test alpha_2_settings --test alpha_2_failures \
  -- --test-threads=1
cargo build --locked --release --features alpha-1-linux \
  --bin zec --bin alpha_1_acceptance --bin alpha_1_bench \
  --bin alpha_2_fixture_lsp --bin alpha_2_acceptance --bin alpha_2_bench
mkdir -p target/alpha-2/alpha-1
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/alpha_1_acceptance --zec ./target/release/zec \
  --repo . --assert --report target/alpha-2/alpha-1/acceptance.json
./target/release/alpha_1_acceptance \
  --verify-report target/alpha-2/alpha-1/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/alpha_1_bench --zec ./target/release/zec \
  --assert --report target/alpha-2/alpha-1/benchmark.json
./target/release/alpha_1_bench \
  --verify-report target/alpha-2/alpha-1/benchmark.json
timeout --signal=TERM --kill-after=5s 45m \
  ./target/release/alpha_2_acceptance --zec ./target/release/zec \
  --lsp ./target/release/alpha_2_fixture_lsp --assert \
  --report target/alpha-2/acceptance.json
./target/release/alpha_2_acceptance \
  --verify-report target/alpha-2/acceptance.json
timeout --signal=TERM --kill-after=5s 20m \
  ./target/release/alpha_2_bench --zec ./target/release/zec \
  --lsp ./target/release/alpha_2_fixture_lsp --assert \
  --report target/alpha-2/benchmark.json
./target/release/alpha_2_bench \
  --verify-report target/alpha-2/benchmark.json
(cd target/alpha-2 && find . -type f ! -name SHA256SUMS -print0 \
  | sort -z | xargs -0 sha256sum > SHA256SUMS)
./script/verify-alpha-2-artifact target/alpha-2
```

未実装binaryまたはreportが存在しない状態はFailであり、上記blockをskipする暫定Passは認めない。

## Fixed environment and fixture LSP

- Linux runnerはGitHub-hosted Ubuntu 24.04 x86_64、localeは`C.UTF-8`、terminalは
  `xterm-256color` 120x40とする。
- fixture repositoryはRust file 8件、設定file、definition/reference/rename/code-action/format対象、
  Unicode identifier、CRLF、同名symbol、ignored fileを含む。
- harnessはtemporary `bin` directoryへfixture serverを`rust-analyzer`としてlinkまたはcopyし、
  childだけの`PATH`先頭へ置く。userのglobal Zed/Cargo configはfresh temporary directoryへ分離する。
- fixture serverは`--help`を成功させ、stdio上のLSP 3.17 framingを使う。initialize capability、
  completion、hover、publishDiagnostics、definition、references、prepareRename/rename、codeAction、
  formatting、shutdown/exitを決定的に実装する。
- request/notification logはserver自身がappend-only JSONLへ記録する。oracleは画面、disk manifest、
  server logの三者であり、zec内部stateをtest-only APIから読まない。
- serverのresponse順序、delay、crash、invalid responseはscenario fileで制御する。production zecへ
  scheduler/provider hookを置かない。

## Gates

### B0. Alpha 1 and parity-ledger regression

- Alpha 1のcanonical command setを同じcandidateで再実行し、既存caseを削除、ignore、filterしない。
- `tests/parity_contract.rs`を実行し、全capability ID、delivery mode、milestone、evidence path、
  pinned Zed revisionの整合を確認する。
- `PROJECT_SERVICE_GRAPH`、`COMMAND_PALETTE`、`LANGUAGE_INTELLIGENCE`、
  `LANGUAGE_MULTIBUFFER`、`SETTINGS_KEYMAP`はAlpha 2 promotionでのみ`verified`へ変更する。

### B1. Project is the sole project-service authority

controlled headless caseとactual binaryのtraceで次をassertする。

- repository sessionにつきZed `Project` Entityは1個である。
- WorktreeStore、BufferStore、LspStore、GitStore、TaskStore、DapStore、SettingsObserver、
  ToolchainStoreはそのProjectから取得し、zecが同じrepository用のstoreを並行生成しない。
- Quick Open、project search、open/save/reloadはProject所有の同じWorktree/Buffer identityを使い、
  Alpha 1のalias dedupeとoutside-file境界を維持する。
- scratch Save As後も同じBuffer Entity、Editor、selection、undo historyを維持する。
- Project drop後5秒以内にfixture LSP、watcher、background taskを回収し、descendant processと
  harness fd countがbaselineへ戻る。

実装確認のためのstatic型名だけをoracleにしない。identity、request trace、external effectを検証する。

### B2. Settings, language registration, and command discovery

- global settings、repositoryの`.zed/settings.json`、language overrideをZed SettingsObserver経路で読み、
  precedenceをfixture JSONと完全一致させる。
- settings変更を実行中に検出し、completion有効/無効、format-on-save、tab sizeを再起動なしで反映する。
- built-in native languageに加え、first-line/shebang、file association、injectionをZed registryで解決する。
- `Ctrl-Shift-P`でcommand paletteを開き、現在focusで利用可能なactionだけを決定的に絞り込む。
  action ID、表示名、key binding、enabled stateを持ち、未接続actionを成功表示しない。
- user keymapでcommand palette、LSP action、既存編集actionをrebindでき、default keymapとのprecedenceを
  Zedと同じkey contextで解決する。

### B3. Completion, hover, and diagnostics

actual-binary PTY caseは次を20 fresh processで実行する。

1. marker位置でcompletionを明示起動し、fixture serverの全itemをlabel/detail/kind順のpickerに表示する。
2. filter、上下移動、documentation表示、cancel、commitを操作し、text editとadditional text editを
   Zed Editor transactionとして適用する。`Ctrl-Z` 1回でcommit前へ戻す。
3. request Aを遅延させたまま位置/queryを変更してrequest Bを完了し、その後Aを返してもBのpopupと
   Bufferだけが残ることを確認する。
4. hoverを開き、plain textとMarkdownをterminal-adapted viewに表示し、linkはOSC 8対応時だけ
   hyperlinkとして出し、非対応時もURL textを残す。
5. warning/error/hint diagnosticsをunderlineまたは明示markerで描き、status summary、next/previous、
   detail overlay、project diagnostics一覧を操作する。修正後のstale diagnosticを除去する。

LSP UTF-16位置とZed Buffer/Display、terminal grapheme/cellの変換をUnicode fixtureで検証し、
byte column、Unicode scalar、UTF-16 unitを混同しない。

### B4. Semantic navigation and editable MultiBuffer

- definition、type definition、references、project symbolsをcommand paletteとdefault key bindingから
  実行できる。
- single targetは同じまたは別fileへ移動し、back/forward navigation historyで元のselectionとviewportへ戻る。
- multiple targets、references、project diagnosticsはZed MultiBufferのexcerptとして1 itemへ表示する。
  excerpt header、path、context、cursor、selectionを描画し、source Bufferと内容を複製しない。
- MultiBuffer内の編集、undo、saveは全source Bufferへ反映し、dirty/conflict protectionはAlpha 1と同じにする。
- resultが遅れて届いた時に、閉じたitem、別pane、別projectへ適用しない。

### B5. Rename, code actions, and formatting

- prepareRenameのplaceholderをpromptへ出し、invalid name/error/cancelで本文を変更しない。
- rename WorkspaceEditが既存file 3件へ及ぼすtext editsをpreview MultiBufferに表示し、acceptで適用、
  rejectで無変更、適用後のundoで全Bufferを一貫して戻す。
- create/rename/delete file operationを含むWorkspaceEditは対象pathと作用を事前表示し、明示accept後だけ
  Project APIへ渡す。repository外、symlink escape、special fileは拒否する。
- code action pickerはkind/preferred/disabled reasonを表示し、editとcommandの成功・失敗を区別する。
- document formattingとrange formattingを実行し、format-on-saveは1回のsave requestにつき高々1回、
  formatter failure時はdirty本文を保持してsave失敗を表示する。
- rename、code action、format後のexact manifest、LSP request log、undo/redo結果をoracleと一致させる。

### B6. Overlay, focus, and input routing

command palette、completion、hover、diagnostic detail、code action、rename promptは共通overlay stackを使う。

- 最上位overlayだけがkey/paste/mouse入力を受け、本文shortcutへ漏らさない。
- `Esc`は最上位だけを閉じ、nested overlayを順に戻る。tab/pane切替、resize、LSP redrawでfocusを失わない。
- terminalが区別できないkey chordにはportable bindingを必ず1つ用意する。
- Kitty keyboard protocolまたはmodifyOtherKeysが利用可能なら拡張keyを使い、未対応terminalでは
  capability negotiation後にfallbackをstatusへ表示する。
- overlay result、error、cancelはsession IDとgenerationを持ち、閉じたsessionへのstale completionを捨てる。

### B7. Failure, security, and process lifecycle

各scenarioを20 fresh processで実行する。

- server not found、spawn failure、initialize error、malformed frame、request error、unexpected EOF、crash、
  hang、restartを発生させる。
- error後も既存Buffer、selection、undo、dirty stateを維持し、LSPなしの編集・保存・終了を続行できる。
- hang中のrequest cancel、tab close、project close、quitは250 ms以内にUIへ反映し、quit時は5秒以内に
  childをterminate後killして回収する。
- workspace edit、command、document linkはworktree trustとpath boundaryを検査し、未承認のrepository外
  mutation/process起動を行わない。
- server stderrの巨大出力、10,000 diagnostics/items、64 KiB documentationでqueueと描画memoryをboundedにする。

### B8. Responsiveness and resource envelope

fixture serverのresponse delayを0にして次を測定する。

- Project追加後のdirectory Ready: Alpha 1 startup p95 3,000 ms以下を維持する。
- LSP initializeからlanguage-ready表示: 20 samplesのp95 1,500 ms以下、max 3,000 ms以下。
- completion request flushからpopup complete: 10 warm-up後100 samplesのp95 100 ms以下、max 250 ms以下。
- diagnostics publishからvisible marker: 100 samplesのp95 100 ms以下、max 250 ms以下。
- definition/reference responseからtarget/MultiBuffer visible: 各100 samplesのp95 150 ms以下、max 500 ms以下。
- 3 file rename previewからapply/save完了: 20 samplesのp95 500 ms以下、max 1,500 ms以下。
- 10,000 completion itemsとdiagnosticsを受けてもzec main processのVmHWMは1,610,612,736 bytes以下、
  描画snapshotはviewportとvisible popup rowsにboundedである。

reportはraw sample、nearest-rank統計、VmHWM、request/response ID列、input/apply ID列を保持し、
verify modeが再計算する。retry、warm cacheへのsample差し替え、outlier除外は行わない。

### B9. CI evidence

Alpha 1と同じcandidate `C` / evidence-only promotion `P`方式を使う。

1. `C`のpush eventで`Alpha 2 gate`をrun attempt 1、retry 0で成功させる。
2. acceptance、benchmark、fixture server log、before/after manifest、environmentをartifactへ保存する。
3. `P`は`docs/alpha-2-evidence.json`だけを追加する`C`のdirect childとする。
4. evidence jobはSHA、event、attempt、唯一のrun ID、job conclusion、artifact ID/digest、report digest、
   required case ID、全assert、`P^ == C`、変更pathをGitHub APIとlocal checkoutの双方から検証する。
5. default branch上の`Alpha 2 evidence` job successだけをcanonical Passとする。

promotion commitが追加する`docs/alpha-2-evidence.json`は次の形に固定する。4 reportのdigestは
candidate artifactから計算し、`gate`と`artifact`の座標は同じretryなしrunから取得する。

```json
{
  "contract_version": 2,
  "candidate_sha": "40 hexadecimal digits",
  "gate": {
    "run_id": 0,
    "job_id": 0,
    "run_attempt": 1,
    "run_url": "https://github.com/OWNER/REPO/actions/runs/RUN_ID",
    "job_url": "https://github.com/OWNER/REPO/actions/runs/RUN_ID/job/JOB_ID"
  },
  "artifact": {
    "id": 0,
    "digest": "64 hexadecimal digits"
  },
  "reports": {
    "acceptance_sha256": "64 hexadecimal digits",
    "benchmark_sha256": "64 hexadecimal digits",
    "alpha_1_acceptance_sha256": "64 hexadecimal digits",
    "alpha_1_benchmark_sha256": "64 hexadecimal digits"
  },
  "counts": {
    "acceptance_case_count": 341,
    "failure_scenario_count": 15,
    "fresh_process_runs": 20
  }
}
```

`script/verify-alpha-2-evidence`はdirect-parent関係、変更path、run/job/artifactの一意性とdigest、
全case ID、raw benchmark統計、correlation列、全embedded evidence file、Alpha 1再実行reportを独立に
再検証する。

## Explicit exclusions from Alpha 2 gate

次は長期parity scopeから除外せず、後続milestoneで測定する。

- full project panel、pane split/dock、tab preview/pin/reorder、session restore
- regex/path-filter project replaceと汎用search MultiBuffer
- Git UI、integrated terminal、tasks、debugger、REPL/notebook
- extension gallery、theme/icon、package/update、remote development
- AI、ACP/MCP、edit prediction、collaboration、media bridge
- terminal image、terminal-aware soft wrap、完全なmouse multi-selection
- macOSのactual-binary gateとplatform別package

Alpha 2実装中にこれらを追加してもよいが、B0〜B9のPass条件を代替しない。

## Implementation order

1. parity ledgerとfailing contract testsを追加する。
2. current `FileServices`をZed `Project`所有storeへ移し、Alpha 1をgreenへ戻す。
3. full language/settings initializationとfixture LSPを接続する。
4. action registry、command palette、共通overlay stackを作る。
5. completion/hover/diagnosticsを接続する。
6. navigation、MultiBuffer、rename/code action/formatを接続する。
7. failure/process/security/performance gateを閉じる。
8. hosted candidateとevidence-only promotionをretryなしで通す。
