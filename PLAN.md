# codetrail — 계획서 (v1)

경량 commit-diff 뷰어 + AI 변경 이유 추적. VS Code 대체(diff/검색/간단 편집 한정).
위치: `/home/dmkimd/project/codetrail` · 바이너리: `codetrail` (CLI+GUI 단일, 서브커맨드)

## 0. 가능성 / 유효성 판단

| 항목 | 판단 | 근거 |
|---|---|---|
| 가능성 | 가능 | git CLI 위임 + ripgrep 계열 크레이트(ignore/grep-*)로 검색, egui로 GUI. 이 머신에 toolchain·egui 크레이트 캐시 확인됨 |
| 유효성: 경량 diff 뷰어 | 유효, 단 단독으론 약함 | gitui/lazygit(TUI, 사용자가 거부), Fork/GitButler(범용 Git GUI)가 이미 있음. 차별점은 3~4번(AI 이유 추적 + AI에게 바로 질문) |
| 유효성: AI 이유 추적 | 부분 중복 | `git-ai`(git-ai-project, 현재 OpenAI 소속)가 이미 "라인 ↔ agent/model/prompt" 연결을 git notes로 제공. 우리는 (a) GUI 안에서 코드 위치 ref → AI 질의까지 한 동선, (b) 사용자가 지정한 압축 바이너리 저장, (c) 저장 포맷을 단순하게 소유 — 이 3가지로 차별화. **git-ai notes 임포트는 v1 범위 밖, 포맷에 확장 여지만 둠** |
| 핵심 리스크 | "왜 바꿨는지"를 에이전트가 안 남길 수 있음 | hook이 편집 사실·세션·transcript 위치는 강제 기록(reason 없어도 복원 가능), reason 텍스트는 skill 지시 + `codetrail note`로 보강 |

**결론: 진행 가치 있음.** 뷰어 단독이 아니라 "diff 뷰어 + ref→AI 질의 + 구조화 이유 저장"이 한 세트일 때만 유효.

## 1. 목표 / 범위 밖 / 수용 기준

기능 요구(사용자 원문 1~6) → 수용 기준(AC):

- AC1 commit별 change: 커밋 목록(무한 스크롤), 커밋 선택 시 파일 트리 + unified/side-by-side diff, 이름변경/바이너리 표시.
- AC2 비교 base 지정: base ∈ {parent(기본), 임의 ref(브랜치/태그/SHA), merge-base(<ref>), 이전 선택 커밋, working tree}. 범위 비교 `base..target` 지원. 상단 바에서 변경 가능, 현재 base가 항상 보임.
- AC3 코드 위치 ref: `path:L1-L2@<commit|worktree>` 형식 ref를 diff/blame/검색 어디서든 라인 선택으로 생성 → 클립보드 복사 또는 "Ask AI"로 즉시 전달. 해당 위치의 AI 이유 기록을 인스펙터에 표시.
- AC4 저장+플러그인: `codetrail install` 이 Claude Code / omp / Codex 에 hook·skill을 설치. AI가 코드를 수정하면 `.git/codetrail/` 에 압축 바이너리 레코드가 쌓이고, 앱이 읽어 표시. 문자열 로그(jsonl) 아님.
- AC5 간단 편집: 파일 열기·편집·저장(Ctrl+S)·외부 변경 감지·diff에서 바로 편집으로 이동. 1MB 초과·바이너리는 읽기 전용.
- AC6 빠른 탐색: (a) 파일명 퍼지(Ctrl+P), (b) 전체 텍스트(Ctrl+Shift+F, 리터럴/정규식/대소문자), (c) blame 탐색(파일 라인 blame + "이 라인의 이력 따라가기" + 커밋/작성자 검색).
- NFR1 UI/UX: 색 전체가 TOML 테마로 커스터마이즈, 버튼 내부 글자 패딩·간격이 일관(아래 §6 토큰), 한글 정상 렌더링.

범위 밖(v1): 커밋/푸시/브랜치 조작(읽기 전용 git), LSP, 플러그인 마켓, 원격 서버 공유, git-ai notes 임포트, Windows/macOS 패키징(코드는 크로스플랫폼 유지, 검증은 Linux만).

## 2. 기술 선택

| 영역 | 선택 | 이유 / 탈락안 |
|---|---|---|
| 언어 | Rust 2021 workspace | 요청 |
| GUI | **egui/eframe (glow 백엔드)** | Tauri는 `libwebkit2gtk-4.1-dev` 미설치 + sudo 비번 필요로 이 머신에서 즉시 빌드 불가, 런타임도 WebView로 더 무거움. egui는 단일 바이너리·RAM 수십 MB·가상 스크롤 쉬움·기존 프로젝트(rosbridge-mirror-egui)에서 검증됨. 탈락: iced/slint(캐시 없음, 커스텀 텍스트 에디터 약함) |
| git | `git` CLI 서브프로세스 (`log -z`, `diff --raw -z`/`-U`, `blame --porcelain`, `cat-file --batch`, `merge-base`) | 정확도 최상(rename/copy/whitespace 옵션 동일), gix/git2 API 학습·버전 리스크 회피. 병목 시 cat-file --batch 상주 프로세스로 최적화 |
| 검색 | `ignore`(워커 병렬 walk, .gitignore) + `grep-regex`/`grep-searcher` + `nucleo-matcher`(파일 퍼지) + `rayon` | ripgrep 동일 엔진 |
| diff 인트라라인 | `similar` | 단어 단위 강조 |
| 구문 강조 | `syntect` (`default-fancy`, 순수 Rust) | tree-sitter는 문법 크레이트 다수 → 빌드 무거움 |
| 저장 직렬화 | `postcard` + `zstd`(reason 텍스트 블록) + `crc32fast` | 컴팩트 바이너리 |
| CLI | `clap` | |
| 설정 | `toml` + `serde` (`~/.config/codetrail/config.toml`, `theme.toml`) | |

한글: egui 기본 폰트에 Hangul 없음 → 시작 시 폰트 탐색(`/usr/share/fonts` 스캔, 우선순위: Pretendard > D2Coding > Noto Sans CJK KR) 후 fallback 체인에 추가. 이 머신에 Noto Sans CJK 확인됨. IME 입력은 egui/winit 지원 범위, 실패 시 한글 **표시**는 보장·입력은 알려진 제한으로 보고.

## 3. 아키텍처

```
codetrail/                        (cargo workspace)
  crates/
    ct-core/    git 읽기 래퍼 + diff 파서 + 검색 + ref 문법      [슬라이스 A]
    ct-store/   AI 이유 레코드 바이너리 저장·조회·앵커 매칭          [슬라이스 B]
    ct-agent/   install(hook/skill), ask(AI 호출), hook 핸들러     [슬라이스 B]
    ct-app/     egui GUI + CLI 엔트리(bin: codetrail)             [슬라이스 C]
  assets/skills/codetrail/SKILL.md   (설치되는 skill 원본)
```

의존: ct-app → ct-core, ct-store, ct-agent. ct-agent → ct-store, ct-core. ct-core는 독립.
모든 git/IO 호출은 UI 스레드 밖(백그라운드 스레드 + `std::sync::mpsc`, 결과 도착 시 `ctx.request_repaint()`).

### 3.1 ct-core 공개 API (계약 — 슬라이스 간 고정)

```rust
pub struct Repo { pub root: PathBuf, pub git_dir: PathBuf }
impl Repo {
  pub fn open(path: &Path) -> Result<Repo>;
  pub fn head(&self) -> Result<String>;                       // full sha
  pub fn refs(&self) -> Result<Vec<RefInfo>>;                 // branches/tags/remotes
  pub fn log(&self, q: &LogQuery) -> Result<Vec<CommitMeta>>; // skip/limit 페이지네이션
  pub fn resolve(&self, rev: &str) -> Result<String>;
  pub fn merge_base(&self, a: &str, b: &str) -> Result<String>;
  pub fn diff(&self, base: &Base, target: &Target, opts: &DiffOpts) -> Result<DiffSet>; // 파일 목록(stat)만
  pub fn file_diff(&self, base: &Base, target: &Target, path: &str, opts: &DiffOpts) -> Result<FileDiff>; // hunks, 지연 로드
  pub fn show_file(&self, rev: &str, path: &str) -> Result<Vec<u8>>;
  pub fn blame(&self, rev_or_worktree: &Target, path: &str, range: Option<(u32,u32)>) -> Result<Vec<BlameLine>>;
  pub fn file_history(&self, path: &str, line_range: Option<(u32,u32)>, limit: usize) -> Result<Vec<CommitMeta>>; // git log -L
  pub fn list_files(&self) -> Result<Vec<String>>;            // git ls-files + untracked(비ignored)
}
pub enum Base { Parent, Rev(String), MergeBaseWith(String), WorkingTree }
pub enum Target { Commit(String), Rev(String), WorkingTree }
pub struct CommitMeta { pub sha: String, pub parents: Vec<String>, pub author: String, pub email: String, pub time: i64, pub subject: String, pub refs: Vec<String> }
pub struct DiffSet { pub files: Vec<FileStat> }   // path, old_path, status(A/M/D/R/C), add, del, binary
pub struct FileDiff { pub path: String, pub old_path: Option<String>, pub hunks: Vec<Hunk> }
pub struct Hunk { pub old_start: u32, pub old_len: u32, pub new_start: u32, pub new_len: u32, pub lines: Vec<DiffLine> }
pub struct DiffLine { pub kind: LineKind /*Ctx|Add|Del*/, pub old_no: Option<u32>, pub new_no: Option<u32>, pub text: String }
pub struct BlameLine { pub line_no: u32, pub sha: String, pub author: String, pub time: i64, pub summary: String, pub orig_line: u32, pub orig_path: String }

// 검색
pub struct SearchQuery { pub pattern: String, pub regex: bool, pub case_sensitive: bool, pub globs: Vec<String> }
pub struct SearchHit { pub path: String, pub line: u32, pub col: u32, pub text: String, pub ranges: Vec<(u32,u32)> }
pub fn search_content(root: &Path, q: &SearchQuery, cancel: Arc<AtomicBool>, tx: Sender<SearchHit>) -> Result<SearchStats>;
pub struct FileIndex; impl FileIndex { pub fn build(paths: Vec<String>) -> Self; pub fn query(&self, q: &str, limit: usize) -> Vec<(String /*path*/, u32 /*score*/, Vec<u32> /*match idx*/)>; }

// ref 문법
pub struct CodeRef { pub path: String, pub start: u32, pub end: u32, pub at: RefAt /*Commit(sha)|Worktree*/ }
impl CodeRef { pub fn parse(s: &str) -> Result<Self>; pub fn to_string(&self) -> String; } // "src/a.rs:10-24@abc1234" / "@worktree"
```

### 3.2 ct-store: 저장 포맷 (AC4)

위치: `<git_dir>/codetrail/` (작업트리 오염 없음, 클론·브랜치 전환 영향 없음, `git gc` 무관).
- `records.ct`: append-only 프레임 `[magic u32 "CTR1"][len u32][crc32 u32][postcard payload]`. 동시 기록은 `O_APPEND` + `flock`. 손상 프레임은 건너뛰고 다음 magic까지 resync.
- `strings.ct`: 경로/세션/agent 문자열 인턴 테이블(레코드는 u32 id만 보유).
- reason 본문: 레코드 내부 `Vec<u8>` = zstd(level 3)(UTF-8). 인덱스는 앱 시작 시 mmap 없이 순차 스캔(레코드 수 ≤ 수십만 가정) 후 메모리 `HashMap<path_id, Vec<RecordRef>>`; 스캔 결과는 `index.ct` 캐시(레코드 파일 길이+mtime 키).

```rust
pub struct Record {
  pub id: u128,                 // time-ordered (ulid-like)
  pub ts_ms: i64,
  pub agent: Agent,             // Claude|Omp|Codex|Other(u16)
  pub session: u32,             // interned
  pub kind: Kind,               // Edit|Create|Delete|Rename|Note
  pub edits: Vec<EditSpan>,     // path(interned), new_start, new_len, anchor
  pub reason: Vec<u8>,          // zstd text; 비어있을 수 있음(hook-only)
  pub transcript: Option<TranscriptPtr>, // path(interned) + byte offset/message idx — reason 부재 시 복원 경로
}
pub struct EditSpan { pub path: u32, pub new_start: u32, pub new_len: u32, pub post_hash: u64 /* xxh3 of added lines text, 정규화(trim-end) */, pub pre_hash: u64 }
```
커밋 연결(rebase/amend 견고): 앱이 `file_diff`의 hunk 추가 라인 해시(`post_hash` 동일 방식)로 레코드와 매칭, 불일치 시 `path + ts` 가 두 커밋 시각 사이인 레코드로 폴백(낮은 신뢰도 배지 표시). 레코드는 SHA를 저장하지 않는다.

API: `Store::open(git_dir)`, `append(Record)`, `for_path(path) -> Vec<Record>`, `for_range(path, start, end, hash_hint)`, `match_hunk(path, added_lines) -> Vec<(Record, Confidence)>`, `decode_reason(&Record) -> String`.

### 3.3 ct-agent: 플러그인 / AI 질의

CLI(모두 `codetrail` 서브커맨드):
- `codetrail install [--claude] [--omp] [--codex] [--dry-run]`: 멱등. 변경 파일 목록을 출력, 기존 설정은 백업 후 병합(덮어쓰기 금지).
  - Claude Code: `~/.claude/settings.json`의 `hooks.PostToolUse`(matcher `Edit|Write|MultiEdit|NotebookEdit`)에 `codetrail hook claude` 추가 + `~/.claude/skills/codetrail/SKILL.md`.
  - omp: `~/.agents/skills/codetrail/SKILL.md`(omp가 로드). hook 대체 수단 없으면 skill 지시만 + `codetrail note`.
  - Codex: `~/.codex/AGENTS.md`에 마커 블록(`<!-- codetrail BEGIN/END -->`) 삽입.
- `codetrail hook claude`: stdin JSON(`session_id`, `transcript_path`, `tool_name`, `tool_input`, `cwd`)을 읽어 repo 판별(해당 cwd가 git repo 아니면 즉시 0 종료), Edit/Write의 변경 범위·해시를 계산해 Record append(reason 비움, transcript ptr 채움). **실패해도 항상 exit 0, 지연 <30ms 목표** (에이전트 흐름 방해 금지).
- `codetrail note --reason "..." [--path P --lines a-b] [--agent X --session S]`: 직전 hook 레코드(같은 세션·같은 파일)에 reason 병합하는 Note 레코드 append.
- `codetrail ask <CodeRef> [--question Q] [--agent claude|omp|codex] [--print]`: 프롬프트 구성(코드 ref 본문 + 해당 diff + 매칭된 reason들 + transcript 요약 포인터) 후 `claude -p` / `omp -p` / `codex exec`에 전달. `--print`는 프롬프트만 출력(테스트·복사용). 앱의 Ask AI 버튼은 동일 함수를 호출하고 stdout을 스트리밍해 패널에 표시.
- skill 내용(요지): "파일을 수정한 직후, 변경의 *이유*를 1~3문장으로 `codetrail note --reason`으로 기록. 무엇이 아닌 왜. 사용자 요청 원문 요약 포함."

## 4. 성능 예산 (측정해서 보고)

| 시나리오 | 예산 |
|---|---|
| 콜드 오픈 → 첫 커밋 200개 표시 (10만 커밋 repo) | < 150ms |
| 커밋 클릭 → 파일 목록 | < 60ms |
| 파일 diff 첫 hunk 표시 (1만 라인 diff) | < 100ms, 스크롤 60fps(행 가상화) |
| 파일명 퍼지 (캐시 인덱스, 5만 파일) | 키 입력당 < 16ms |
| 전체 텍스트 검색 (1만 파일 repo, 리터럴) | 첫 결과 < 100ms, 완주 < 400ms, 취소 즉시 |
| 유휴 RSS | < 150MB (repo 중간 크기) |
| hook 실행 | < 30ms |

## 5. 단계 / 슬라이스 / 파일 소유

선행(메인): 워크스페이스 스캐폴드 + 계약(§3.1~3.3 타입 스텁) 커밋 → 이후 3슬라이스 병렬.

- **A (ct-core)**: `crates/ct-core/**` 전부. 단위+통합 테스트(임시 repo 생성: merge, rename, 바이너리, 한글 경로, 공백 경로 `-z` 처리). 검색·퍼지 벤치.
- **B (ct-store + ct-agent)**: `crates/ct-store/**`, `crates/ct-agent/**`, `assets/skills/**`. 테스트: 프레임 손상 resync, 동시 append, install 멱등+백업, hook 입력 fixture, ask `--print` golden.
- **C (ct-app)**: `crates/ct-app/**`. A·B 계약 타입만 사용(스텁 위에서 시작, 구현 도착 후 통합). 테마/폰트/레이아웃/위젯/에디터/키바인딩.
- 통합(메인+검증 워커): 전체 빌드, E2E 시나리오(§7), 성능 측정, 스크린샷 검토.

## 6. UI/UX 사양 (Operate 모드: 스캔 가능성·일관성이 표현보다 우선)

- 레이아웃: 좌 레일(Commits / Search / Files 탭) · 중앙 diff · 우 인스펙터(Why / Blame / Ask AI) · 상단 바(repo, **Base 칩** `base: parent ▾`, 범위 `A..B`, 뷰 토글 unified/split) · 하단 상태줄(ref, 검색 통계, 인덱스 상태).
- 키: Ctrl+P 파일, Ctrl+Shift+F 검색, Ctrl+K 팔레트(커밋/ref/동작), Ctrl+B base 변경, Ctrl+E 편집 토글, Ctrl+S 저장, `[`/`]` 이전/다음 hunk, `j`/`k` 커밋 이동, Ctrl+Shift+C ref 복사, Ctrl+L Ask AI.
- **디자인 토큰** (`theme.toml`로 전부 오버라이드): 간격 스케일 4/8/12/16/24; 컨트롤 높이 28(보통)/24(컴팩트); **버튼 패딩 가로 12 · 세로 (높이−line_height)/2, 라벨 수직 중앙, 아이콘-라벨 간격 6, 최소폭 = 라벨+24**; 행 높이 22(diff), 커밋 행 40(2줄); 반경 6/4; 포커스 링 2px accent. 색 토큰: `bg.base/raised/sunken`, `fg.primary/muted/disabled`, `accent`, `border`, `diff.add.bg/fg`, `diff.del.bg/fg`, `diff.add.word`, `diff.del.word`, `ai.badge`, `warn`. 대비 비율 본문 ≥ 4.5:1 (코드로 검사하는 단위 테스트 포함).
- 타이포: UI = 시스템 sans(Pretendard→Noto Sans CJK KR), 코드 = 모노(JetBrains Mono→D2Coding→DejaVu Sans Mono) + Noto CJK 폴백. 크기 UI 13 / 코드 13, 설정에서 조정. 탭 너비 4.
- 상태: 로딩 스켈레톤, 빈 상태 문구, 오류는 인라인 배너(재시도 버튼), 바이너리/대용량 diff 접힘 + "그래도 열기".
- 기본 테마 2종(어두운 기본/밝은), 색 변경은 설정 패널에서 실시간 반영 + 파일 저장.
- 참조 skill 적용: impeccable(`craft-floor`: 대비·간격·상태 floor), gpt-taste/design-taste는 웹 랜딩 전용 지시(GSAP/AIDA)라 **타이포 위계·여백·버튼 대비 원칙만 차용**, 모션은 최소(hover/selection 100~150ms).

## 7. 검증 계획

1. 단위·통합: `cargo test --workspace` (각 슬라이스 소유 범위).
2. 실 repo E2E (스크립트화): 이 머신의 실제 repo(`~/project/loudness-lab`, 대형 후보 하나 추가 선정)로
   - 커밋 목록/diff가 `git show`·`git diff` 출력과 라인 단위 일치(자동 비교 스크립트).
   - base를 parent / 임의 ref / merge-base / range로 바꿔 파일 목록이 `git diff --name-status`와 일치.
   - 검색 결과 집합이 `rg -n` 결과와 일치(순서 무시).
   - blame 결과가 `git blame --porcelain`과 일치.
3. AI 추적 E2E: 임시 repo에서 `codetrail install --claude --dry-run` → 실제 설치(임시 HOME) → 모의 hook JSON 주입 → `note` → 앱 로직(`match_hunk`)이 커밋 후 hunk에 레코드를 연결 → `ask --print`가 ref·diff·reason 포함.
4. GUI 스모크: `codetrail --smoke <repo> --screenshot out.png`(헤드리스 대체: Xvfb) 로 주요 화면(커밋 diff / split / 검색 / 인스펙터 Why / 테마 밝음) 캡처 → 이미지 직접 확인(버튼 패딩·한글·대비). **1 배치 점검 → 일괄 수정 → 최대 1회 재확인.**
5. 성능: §4 표 항목을 `cargo bench` 또는 `--bench-startup` 플래그로 측정해 보고(미달은 미달로 기재).
6. 독립 리뷰: 이 계획서 → 다른 model family(codex)로 1회 검토, 구현 완료 후 diff 1회 검토. 지적은 재검증 후 반영.

## 8. 위험 / 결정 기록

- egui 에디터 한계: 큰 파일/긴 라인 성능, 한글 IME → 1MB 제한·읽기 전용 폴백, IME는 실측 후 보고.
- git 서브프로세스 비용: 커밋 클릭마다 spawn(수 ms) 수용. 병목 시 `cat-file --batch` 상주.
- hook이 에이전트 UX를 해치는 위험 → 항상 exit 0, 타임박스, 실패는 `<git_dir>/codetrail/errors.log`에만.
- 레코드-커밋 매칭 오탐: 신뢰도(해시 일치=High / 시간창 폴백=Low) UI에 노출, Low는 "추정" 라벨.
- `~/.claude/settings.json` 병합 사고 방지: 백업 + 멱등 + `--dry-run` + 테스트.
- 이 머신에서 GUI 실행은 `DISPLAY=:1` 사용 가능, 헤드리스는 Xvfb.

---

# §9 v2 — 독립 리뷰(codex) 반영. **본문과 충돌 시 §9가 우선.**

반영하지 않은 지적: (1) "v1 범위 축소"는 사용자가 기능 1~6을 모두 요구했으므로 기각 — 대신 **게이트 순서**로 구현(G1 core+diff+base → G2 검색/blame → G3 store+hook+ask → G4 편집/테마 마감), 각 게이트 통과 후 다음 진행. omp/Codex 설치는 G3에서 "skill/AGENTS 설치 + note" 수준(hook은 Claude만). (2)의 "non-UTF-8 경로 byte 지원"은 기각: `RepoPath = String`(lossy), `-z` 파싱만 정확히. 사유: Linux repo의 비-UTF-8 경로는 희귀, 계약 복잡도 대비 이득 낮음. 한계로 문서화.

## 9.1 ct-core 계약 변경
```rust
pub type Oid = String;                       // 40-hex
pub enum Treeish { Commit(Oid), Parent { commit: Oid, n: u8 /*1-based*/ }, Index, Worktree }
pub struct Comparison { pub old: Treeish, pub new: Treeish }   // Base 지정 UI는 Comparison을 만든다
// UI base 모드 → Comparison 생성 헬퍼: Comparison::parent_of(commit), ::range(a,b), ::merge_base(a,b,&Repo), ::commit_vs_worktree(c)
// 잘못된 조합(Worktree vs Worktree)은 생성 시 Err.
pub fn diff(&self, c: &Comparison, o: &DiffOpts) -> Result<DiffSet>;
pub fn file_diff(&self, c: &Comparison, path: &str, o: &DiffOpts) -> Result<DiffFile>;
pub struct DiffFile {
  pub path: String, pub old_path: Option<String>,
  pub old_oid: Option<Oid>, pub new_oid: Option<Oid>,
  pub old_mode: Option<u32>, pub new_mode: Option<u32>,
  pub kind: FileKind /*Text|Binary|Symlink|Submodule*/, pub status: Status,
  pub hunks: Vec<Hunk>,
}
pub struct DiffLine { /* 기존 */ pub no_newline_at_eof: bool }
// merge commit: 기본 first-parent(Parent{n:1}) diff, UI에 "merge: parent 1/2 선택" 제공. combined diff(-c/--cc)는 미지원이며 명시적으로 표시.
```
`resolve()`는 `Oid`를 반환. root commit(부모 없음)은 빈 트리 대비로 처리. shallow/worktree/서브모듈 repo 열기는 오류 없이 동작해야 함.

## 9.2 비동기 계약 (ct-app 소유, 모든 슬라이스가 따름)
- 고정 워커 풀(스레드 4) + 요청마다 `JobId{kind, generation}`; 새 요청 시 같은 kind의 이전 generation 결과는 폐기, 이전 job의 `cancel: Arc<AtomicBool>`을 set. 결과 채널은 bounded(1024), 가득 차면 검색 job은 중단.
- git 서브프로세스는 timeout(기본 30s, 설정 가능) + 취소 시 kill. UI는 항상 상태(loading/error/empty) 표시.

## 9.3 ct-store 저장 포맷 (v1 본문 §3.2 대체)
- 단일 파일 `<git_dir>/codetrail/log.ct` **self-contained** (strings.ct 제거). 파일 헤더 `"CTLG"+version u16`.
- 프레임: `[magic "CTF1"][len u32 (≤1MiB)][seq u64][crc32 of payload][postcard payload]`. 프레임 전체를 **한 번의 write_all**, `flock(LOCK_EX)` 구간에서 기록. fsync 없음(손실 허용, 문서화), 손상 프레임은 magic 재동기화 + len 상한 + crc로 방어, 스킵 카운트를 앱 상태줄에 노출.
- 프레임 종류: `Edit`, `Note{record_id}`, `Unattributed`. 경로/세션은 프레임 내 문자열(디스크 크기보다 견고성 우선; reason만 zstd).
- 인덱스 캐시 `index.ct`는 임시파일+rename으로 원자 교체, 헤더에 log 길이·mtime·version, 불일치 시 폐기·재생성. 버전 미일치 시 읽기 전용 + 안내.
- `codetrail export [--json]`: 전 레코드 덤프(백업/재clone 대비). 데이터는 **로컬 전용**(.git 삭제 시 소실) — README에 명시.
- Edit 레코드 필드 추가: `head_at_edit: Oid`, `pre_blob: Option<Oid>`(편집 전 파일 blob, `git hash-object`), `post_blob: Oid`, `tool_use_id: String`, `fingerprint: u64`(추가 라인 정규화 해시), `occurrence: u16`(동일 해시 중 순번). 시간 기반 폴백 **삭제**.
- 커밋 연결(결정적): ① `post_blob`이 어떤 커밋의 해당 파일 blob과 같으면 **High**, ② 커밋 hunk의 추가 라인 fingerprint 일치 **Medium**, ③ 그 외 `Unknown`(UI에서 수동 연결 불가 시 "연결 안 됨" 표기). 삭제-only 변경은 ①만.

## 9.4 Claude Code hook 계약
- 설치 항목 3개: `PreToolUse`(matcher `Edit|Write|MultiEdit|NotebookEdit`)에서 대상 파일 pre_blob 스냅샷을 `<git_dir>/codetrail/pending/<tool_use_id>` 에 저장, `PostToolUse`(같은 matcher)에서 pending을 읽어 Edit 레코드 기록 후 `additionalContext`로 `codetrail record <id> recorded; run: codetrail note --record <id> --reason "..."` 를 에이전트에 반환, `PostToolUseFailure`에서 pending 삭제, `Stop`에서 워크트리 diff 중 레코드로 커버되지 않은 변경(Bash/MCP 수정)을 `Unattributed` 로 기록.
- 각 hook 엔트리: `{"type":"command","command":"codetrail hook claude <event>","timeout":5}`; 중복 설치 방지 키는 command 문자열 일치. 실제 settings.json 스키마와 stdin 필드명은 구현 전에 Claude Code 공식 hooks 문서/현행 `~/.claude/settings.json`에서 **확인 후** 작성(추측 금지).
- `note`는 `--record <id>` 필수(없으면 `--session`+`--path` 최근 1건, 모호하면 거부).

## 9.5 Ask AI 안전
- 전송 직전 **프롬프트 미리보기** 다이얼로그(기본 on, 설정에서 끔), 포함 항목(코드/diff/reason/transcript 요약) 체크박스, `.env`·키처럼 보이는 패턴(간단 정규식)은 경고 후 기본 제외. 타임아웃·취소 버튼 필수.

## 9.6 검증 기준 구체화
- 측정 표준: 고정 fixture 2종 — F1(중간): `loudness-lab`급 실 repo, F2(대형): 합성 repo 생성 스크립트(커밋 20k, 파일 10k, 총 ≈200MB, 긴 줄(1만자) 파일 포함). cold(`echo 3 > drop_caches` 불가 시 첫 실행)/warm 구분, 각 시나리오 20회 p50/p95 보고, 예산은 **p95 기준**.
- 자동 E2E 실패 시나리오 표(전부 테스트로): stale job 결과 폐기 / 검색 취소 / store 손상 프레임 복구 / 동시 append 2프로세스 / 편집 중 외부 변경 충돌 감지 / 원자 저장(임시+rename) / Ask AI 타임아웃·취소 / 설정·테마 파일 파손 시 기본값 롤백 / root commit·merge commit·shallow·worktree·서브모듈·rename·binary·no-newline / 한글·공백 경로.
- IME는 실기 확인 불가 시 "미검증"으로 보고.

---

# §10 v3 — 차별점 재정의: "아주 가벼운 데스크탑 + 백그라운드 AI agent(pi 기반) + 구독 로그인/모델 선택"

## 10.1 조사로 확인한 사실 (이 세션에서 직접 확인)
- pi = `@earendil-works/pi-coding-agent` (npm, bin `pi`, 구 `@mariozechner/*`/`badlogic/pi-mono`). RPC: `pi --mode rpc` JSONL(LF 구분, stdin 명령/stdout 응답+이벤트). 명령: prompt, steer, follow_up, abort, get_state, set_model, cycle_model, get_available_models, set_thinking_level, new_session 등. 이벤트: agent_start/turn_*/message_update(text_delta)/tool_execution_start·update·end/agent_end/**agent_settled**(완료 판단은 이것). 문서: github.com/badlogic/pi-mono packages/coding-agent/docs/{rpc,rpc-commands,json,providers}.md
- 로컬 스모크(/tmp/pitest): `pi --mode rpc --no-session` 기동 OK, get_state/get_available_models 응답 OK. **미로그인 상태에서 models=[]**. pi 프로세스 1개 유휴 RSS ≈ 147MB(Node) — "가볍다"는 주장은 도구 세트/프롬프트가 작다는 뜻이지 프로세스가 작다는 뜻 아님. 백그라운드 agent 수만큼 곱해짐 → 동시 실행 상한(기본 3) 필요.
- 런타임: pi는 Node ≥22.19 필요(`engines`). 이 머신 node 22.17 → 앱 전용 디렉터리에 npm `node@22` 패키지(22.23.3 확인)로 격리 설치해 사용(전역 node 변경 없음).
- 인증: RPC에 login 명령 없음. 구독 로그인은 `pi-ai login <provider>`(OAuth providers 확인: anthropic / github-copilot / openai-codex) 또는 pi TUI `/login`. 자격증명은 `<agent-dir>/auth.json`(`PI_CODING_AGENT_DIR`로 위치 지정), pi가 자동 refresh.
- **Anthropic 약관(code.claude.com/docs/en/legal-and-compliance)**: 서드파티 개발자는 Claude.ai 로그인 제공 금지, Free/Pro/Max 자격증명으로 사용자 대신 라우팅 금지, **Claude.ai 자격증명/세션 토큰 수집·저장·중개 금지**, 로그인은 Anthropic 자체 플로우로 완료. 예외: 사용자가 *수정되지 않은 Claude Code 바이너리*에 자기 구독으로 로그인하는 것은 허용.

## 10.2 결정 (사용자 요구와의 차이 명시)
1. **Claude 구독**: pi의 anthropic OAuth를 앱에서 노출하지 **않는다**(약관 위반 소지). 대신 백엔드 `claude`(공식 바이너리, 사용자가 `claude auth login`으로 직접 로그인한 상태)를 agent 백엔드로 지원 — 앱은 `claude auth status`만 읽고 토큰을 만지지 않음. 모델은 `claude --model`로 선택(alias/풀네임). Claude를 pi 안에서 쓰려면 API 키(Console)만 허용: `auth.json`에 키 입력 UI.
2. **ChatGPT 구독**: pi의 `openai-codex` OAuth(pi 자체 플로우, 앱은 URL 열기·붙여넣기 코드 전달만, 토큰 파일은 앱이 읽지 않음) 또는 백엔드 `codex`(공식 CLI). GitHub Copilot도 pi 플로우로 자연 지원.
3. 백엔드 추상화 `AgentBackend { Pi, Claude, Codex }` — 공통: 시작/프롬프트/중단/스트림/모델 목록·선택. Pi가 1차(전 기능: 모델 목록, thinking level, steer, 이벤트 상세), Claude/Codex는 `-p`/`exec` 스트림 JSON 기반 최소 기능(프롬프트·중단·모델 선택).
4. 백그라운드 실행: agent run = 독립 프로세스 + 기본 **격리 git worktree**(`~/.local/share/codetrail/worktrees/<repo-id>/<run-id>`, 브랜치 `ct/<slug>`). 사용자의 작업트리는 건드리지 않음. 옵션으로 "현재 작업트리에서 실행". 완료 시 데스크탑 알림 + Runs 목록 배지. 결과 보기 = 기존 diff 뷰어의 Comparison(base..run 브랜치). **Apply** = 작업트리가 깨끗할 때만 `git merge --no-ff ct/<slug>`(충돌 시 abort 후 안내), Discard = worktree+브랜치 삭제(확인 필수).
5. AI 이유 추적 통합: 앱이 실행한 pi run은 hook 없이 `tool_execution_end`(edit/write)에서 Edit 레코드를 직접 기록하고 **reason = 사용자 프롬프트(+assistant 최종 요약 1~3문장)** 로 자동 채움 → Why 패널에서 별도 `note` 없이 바로 보임. 레코드의 agent=Pi, session=run-id.
6. 질문(Ask AI)은 같은 백엔드·모델 선택을 사용(선택 상태 공유), 읽기 전용 모드(`--no-tools` 또는 read-only 도구만)로 실행.

## 10.3 수용 기준 (AC7~AC10)
- AC7 agent 런타임: `codetrail agent setup`(및 GUI 첫 실행 안내)이 앱 전용 dir(`~/.local/share/codetrail/agent/`)에 node@22 + pi를 설치, 버전 고정(lockfile), 재실행 멱등. 이미 PATH에 호환 `pi`가 있으면 재사용 옵션.
- AC8 로그인/모델: 설정 > Accounts 패널에 provider별 상태(Logged in/out, 구독/키). ChatGPT(openai-codex), GitHub Copilot: 앱에서 로그인 시작→브라우저 열기→(리다이렉트 안 닿으면) 코드 붙여넣기→완료 상태 반영, 로그아웃. Claude: 공식 `claude` 로그인 상태 표시 + "터미널에서 `claude auth login` 실행" 안내 버튼(앱이 토큰을 다루지 않음을 UI에 명시). API 키 입력(마스킹, 0600 저장 위치=pi auth.json, 앱 설정 파일엔 저장 안 함). 모델 선택 드롭다운은 get_available_models 결과(로그인된 provider만) + thinking level 선택, 선택값 영속.
- AC9 백그라운드 agent: Runs 패널(실행/대기/완료/실패/중단), 새 run 생성(프롬프트+모델+base 브랜치+격리 여부), 실시간 스트림(텍스트, tool 호출 요약), Abort, steer/follow-up, 동시 실행 상한 설정, 앱을 닫으면 실행 중 run 처리 정책(기본: 종료 전 확인, 종료 시 abort+상태 저장). 완료 알림.
- AC10 결과 연동: run 완료 → 해당 브랜치 diff를 base 비교로 즉시 열기, Why 패널에 자동 reason, Apply/Discard 동작.
- NFR: 앱 자체 유휴 RSS ≤150MB 유지(agent 프로세스 제외), run 0개일 때 pi 프로세스 0개(지연 기동). pi 자식 프로세스는 앱 종료·크래시 시 고아로 남지 않음(프로세스 그룹/`PR_SET_PDEATHSIG`).

## 10.4 보안
- 앱은 OAuth 토큰/API 키를 로그·설정·클립보드·크래시 리포트에 쓰지 않는다(auth.json은 pi 전용, 권한 0600 확인 후 경고). 로그인 로그/stderr는 토큰 패턴 마스킹.
- agent run 프롬프트 전송 전 미리보기는 Ask AI(읽기 전용)에만 필수, Run은 "무엇이 전송되는지"(프롬프트+컨텍스트 파일 목록) 요약 표시.
- worktree 밖 경로 쓰기 금지는 pi의 sandbox가 아니라 cwd 격리일 뿐임을 UI에 명시(pi는 bash 도구로 임의 명령 실행 가능 — 위험 고지, run 생성 화면에 상시 배너).

## 10.5 검증 (추가)
- 실제 pi 프로세스 스모크: 미로그인 상태 get_available_models=[] 처리 UI, 로그인 필요 안내.
- 로그인된 provider가 없는 환경이므로 **LLM 호출 E2E는 가짜 pi(스크립트 모의 RPC 서버, 이벤트 시퀀스 재생)로 자동 테스트**하고, 실제 구독 로그인·실모델 호출은 사용자가 로그인한 뒤 수동 검증 항목으로 명시(미검증 보고).
- run 생명주기 테스트: worktree 생성/정리, 동시 상한 큐잉, abort, 프로세스 고아 없음, 앱 재시작 시 상태 복구, Apply 충돌 abort, Discard 안전장치.

## 10.6 독립 리뷰(codex) 반영 — §10.2/10.3과 충돌 시 이 절이 우선
- 인증 경계 정정: 앱은 "토큰 파일을 읽지 않는다"만 보장, OAuth 코드 붙여넣기 중계는 수행(=자격증명이 앱 프로세스 메모리를 거침을 UI·README에 명시). **API 키 입력 UI는 제거**(환경변수/pi 자체 설정 사용). 코드 중계는 openai-codex/github-copilot에만. Claude는 외부 로그인만(상태 조회 `claude auth status`). provider별 약관 근거와 kill switch(`config.toml: accounts.disabled=["…"]`) 문서화.
- 백엔드 추상화: **공통 최소 계약 + capability matrix**. `Capabilities{ list_models, steer, follow_up, abort, thinking_level, streaming_tools, persistent_session }`; Pi 전부, Claude/Codex는 prompt/abort/model 선택/스트림만(나머지 UI 비활성). 
- worktree는 "VCS 작업 격리"이지 보안 격리가 아님(agent는 홈·자격증명·네트워크 접근 가능) — UI/README 문구 고정. 실행 시 자식 프로세스 env는 화이트리스트(PATH, HOME, LANG, 해당 backend 필요 변수)만 전달(다른 provider 토큰 env 제거).
- 결과 모델: run 시작 시 **base SHA 고정**. agent 종료(settled) 시 앱이 worktree 변경을 `git add -A && git commit`(author `codetrail-run <run-id>`)로 run 브랜치에 아티팩트화(agent가 이미 커밋했어도 추가 변경만 커밋). diff는 `base_sha..run_branch`. Apply = 작업트리 clean + 현재 브랜치 HEAD가 base의 후손일 때 `git merge --no-ff`, 충돌 시 `merge --abort`. 검토(diff) 전 Apply 불가는 아님 — 단 확인 다이얼로그에 변경 파일 수 표시.
- 프로세스: Linux 우선. 자식은 `setsid`로 새 세션/프로세스 그룹, 종료는 abort 명령 → 5s → SIGTERM(그룹) → 3s → SIGKILL(그룹). `PR_SET_PDEATHSIG(SIGKILL)` 병행. run 상태 파일에 pid+starttime(/proc/<pid>/stat) 저장, 앱 재시작 시 reconcile(살아있으면 재attach 불가 → 종료 후 `interrupted`로 표기).
- reason 자동 기록: tool 이벤트가 아니라 **settled 후 worktree diff(base..run 커밋)가 파일/hunk의 진실**. 각 변경 파일마다 Edit 레코드 1건(post_blob=커밋의 blob → High 연결), reason = "run 프롬프트(앞 1000자) + assistant 최종 요약(앞 500자)", 파일에 대한 edit/write tool 이벤트가 있으면 해당 tool-call 직전 assistant 텍스트 조각을 `intent`로 덧붙임. 비밀 패턴은 ct-agent의 secret scan으로 마스킹 후 저장.
- 테스트: backend별 fake executable(스크립트) 계약 테스트(정상/깨진 JSON/인터리브 stderr/크래시/타임아웃/지연 출력), 고정 버전 호환성(pi 0.74.2 확인 기준) 메타 테스트, **실로그인 canary는 opt-in(`CT_LIVE=1`)**: 이 머신에서 `claude`(claude.ai 로그인)와 `codex`(ChatGPT 로그인) 확인됨 → 1-토큰급 프롬프트로 실제 호출 1회씩 수동 검증하고 결과 보고. pi는 미로그인이므로 pi 실호출은 사용자 로그인 후 검증 항목.

## 10.7 슬라이스 / 파일 소유 / 계약
- **D (crate `ct-agentd`, w1)**: 런타임 설치(node@22+pi 격리, 락/버전 고정), pi RPC 클라이언트(LF-only JSONL 프레이머, id 상관, backpressure), Claude/Codex 어댑터(공식 CLI 스트림 JSON 파싱), 모델 목록, 계정 상태, 로그인 플로우(openai-codex/github-copilot 중계, 로그아웃), fake pi/claude/codex 계약 테스트. 소유: `crates/ct-agentd/**`.
- **E (crate `ct-runs`, w2)**: run 레코드 영속(앱 데이터 dir, 재시작 복구), 큐/동시 상한, worktree 생성·정리, 프로세스 supervisor, 완료 시 아티팩트 커밋, Apply/Discard, ct-store 자동 레코드 기록(§10.6). 소유: `crates/ct-runs/**`. ct-agentd의 `AgentSession` 계약만 사용(스텁 위에서 시작).
- **F (ct-app, w3)**: Accounts 패널, 모델/thinking 선택, Runs 패널·새 run 다이얼로그·라이브 스트림·알림, Ask AI를 선택된 백엔드/모델로 라우팅, 레이아웃 미니멀 유지. 소유: `crates/ct-app/**`.
- 계약(공개 API — D가 첫 단계에 `crates/ct-agentd/src/lib.rs`를 컴파일되는 시그니처로 제공):
```rust
pub enum BackendKind { Pi, Claude, Codex }
pub struct Capabilities { pub list_models: bool, pub steer: bool, pub follow_up: bool, pub abort: bool, pub thinking_level: bool, pub streaming_tools: bool, pub persistent_session: bool }
pub fn capabilities(b: BackendKind) -> Capabilities;
pub struct ModelInfo { pub backend: BackendKind, pub provider: String, pub id: String, pub name: String, pub reasoning: bool, pub context_window: u32 }
pub struct ModelSel { pub backend: BackendKind, pub provider: Option<String>, pub id: String, pub thinking: Option<String> }
pub fn list_models(b: BackendKind) -> Result<Vec<ModelInfo>>;   // pi: 로그인된 provider만; claude/codex: 내장 별칭 목록
pub struct SessionOpts { pub cwd: PathBuf, pub model: Option<ModelSel>, pub read_only: bool, pub env_allow: Vec<String>, pub system_note: Option<String> }
pub enum AgentEvent { Started, TextDelta(String), ToolStart{ id: String, name: String, summary: String }, ToolEnd{ id: String, name: String, ok: bool, path: Option<String> }, Usage{ input: u64, output: u64, cost: Option<f64> }, Settled{ ok: bool, error: Option<String> }, Stderr(String), Exited(Option<i32>) }
pub trait AgentSession: Send { fn pid(&self) -> Option<u32>; fn prompt(&mut self, text: &str) -> Result<()>; fn steer(&mut self, text: &str) -> Result<()>; /*Unsupported err if !cap*/ fn follow_up(&mut self, text: &str) -> Result<()>; fn abort(&mut self) -> Result<()>; fn set_model(&mut self, m: &ModelSel) -> Result<()>; fn events(&self) -> &std::sync::mpsc::Receiver<AgentEvent>; fn close(self: Box<Self>); }
pub fn start_session(b: BackendKind, o: SessionOpts) -> Result<Box<dyn AgentSession>>;
pub enum AccountState { LoggedIn{ method: String }, LoggedOut, Unavailable{ reason: String } }
pub struct Account { pub id: String /*"openai-codex","github-copilot","claude","codex"*/, pub label: String, pub state: AccountState, pub login: LoginKind /*InApp|ExternalCli{ command: String }|None*/ }
pub fn accounts() -> Vec<Account>;
pub enum LoginEvent { OpenUrl(String), NeedCode{ prompt: String }, Progress(String), Done, Failed(String) }
pub struct LoginHandle { pub events: Receiver<LoginEvent>, /* submit_code(&self,&str), cancel(&self) */ }
pub fn login_start(account_id: &str) -> Result<LoginHandle>; pub fn logout(account_id: &str) -> Result<()>;
pub struct RuntimeStatus { pub installed: bool, pub node: Option<String>, pub pi: Option<String> }
pub fn runtime_status() -> RuntimeStatus; pub fn runtime_setup(progress: &dyn Fn(&str)) -> Result<()>;
// ct-runs
pub struct RunSpec { pub repo: PathBuf, pub prompt: String, pub model: ModelSel, pub base_ref: String, pub isolate: bool }
pub enum RunState { Queued, Running, Succeeded, Failed(String), Aborted, Interrupted }
pub struct RunInfo { pub id: String, pub spec: RunSpec, pub base_sha: String, pub branch: Option<String>, pub worktree: Option<PathBuf>, pub state: RunState, pub started_ms: i64, pub ended_ms: Option<i64>, pub files_changed: u32 }
pub struct RunManager; impl RunManager { pub fn open(data_dir: &Path, max_concurrent: usize) -> Result<Self>; pub fn submit(&self, spec: RunSpec) -> Result<String>; pub fn abort(&self, id:&str)->Result<()>; pub fn steer(&self,id:&str,text:&str)->Result<()>; pub fn list(&self)->Vec<RunInfo>; pub fn subscribe(&self)->Receiver<RunUpdate /*State(RunInfo)|Event(id,AgentEvent)*/>; pub fn apply(&self,id:&str)->Result<ApplyOutcome>; pub fn discard(&self,id:&str)->Result<()>; pub fn comparison(&self,id:&str)->Result<(String /*base_sha*/, String /*head_sha*/)>; }
```
