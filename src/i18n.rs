//! Languages for the human-facing text: `kb help`, the viewer's status line
//! and prompts, the on-page note bubble, and a one-line explanation of each
//! error code.
//!
//! What agents and scripts parse stays English and stable in every language:
//! command names, `ok ...` result lines, JSON output, error codes, the
//! `error[code]: message` line and snapshots.
//!
//! English is the default. Another language is chosen with `--lang <code>`,
//! `KB_LANG`, or `kb lang <code>` (remembered in the state dir); `auto`
//! follows the operating system's locale.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    En,
    Ko,
    Ja,
    Zh,
    Es,
    Fr,
    De,
}

pub const CODES: [&str; 7] = ["en", "ko", "ja", "zh", "es", "fr", "de"];
const ALL: [Lang; 7] = [Lang::En, Lang::Ko, Lang::Ja, Lang::Zh, Lang::Es, Lang::Fr, Lang::De];

/// "ko", "ko_KR.UTF-8", "ko-KR", "Korean_Korea.949" ... -> Lang. Unknown: None.
pub fn parse(s: &str) -> Option<Lang> {
    let s = s.trim().to_ascii_lowercase();
    let head: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let by_name = ["english", "korean", "japanese", "chinese", "spanish", "french", "german"];
    CODES.iter().position(|c| *c == head).or_else(|| by_name.iter().position(|n| *n == head)).map(|i| ALL[i])
}

/// Resolve a setting: a language code, or "auto" for the OS locale.
fn resolve(setting: &str) -> Option<Lang> {
    if setting.trim().eq_ignore_ascii_case("auto") {
        return Some(parse(&crate::platform::os_locale()).unwrap_or(Lang::En));
    }
    parse(setting)
}

/// Is this something `kb lang <x>` / `--lang <x>` accepts?
pub fn valid(setting: &str) -> bool {
    setting.trim().eq_ignore_ascii_case("auto") || parse(setting).is_some()
}

/// The language in effect: `KB_LANG` (set by `--lang` too), then the saved
/// choice, then English.
pub fn lang() -> Lang {
    static L: OnceLock<Lang> = OnceLock::new();
    *L.get_or_init(|| {
        let env = std::env::var("KB_LANG").ok().filter(|v| !v.trim().is_empty());
        let saved = || std::fs::read_to_string(crate::platform::home_dir().join("lang")).ok();
        env.or_else(saved).and_then(|s| resolve(&s)).unwrap_or(Lang::En)
    })
}

pub fn code(l: Lang) -> &'static str {
    CODES[l as usize]
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum M {
    /// Viewer: key help shown after the leader key.
    ViewKeys,
    /// Viewer: idle status line. {} = leader letter.
    Idle,
    PromptOpen,
    /// {} = pin number, {} = element.
    PromptPin,
    PinMode,
    /// {} = dialog kind.
    Dialog,
    /// {} = url.
    Opening,
    /// {} = count, {} = directory.
    SavedPins,
    /// {} = reason.
    CannotWritePins,
    /// Placeholder in the on-page note bubble.
    NoteHint,
}

#[cfg(test)]
const KEYS: [M; 10] = [
    M::ViewKeys, M::Idle, M::PromptOpen, M::PromptPin, M::PinMode, M::Dialog, M::Opening, M::SavedPins,
    M::CannotWritePins, M::NoteHint,
];

/// Every message in every language, in `CODES` order.
fn table(m: M) -> [&'static str; 7] {
    match m {
        M::ViewKeys => [
            "1/2/4 layout | arrows pane | z zoom | l url | t/w/n/p tabs | r reload | [ ] history | a badges | m pin | s save pins | q quit",
            "1/2/4 분할 | 화살표 창 이동 | z 줌 | l 주소 | t/w/n/p 탭 | r 새로고침 | [ ] 뒤/앞 | a 배지 | m 핀 | s 핀 저장 | q 종료",
            "1/2/4 分割 | 矢印 ペイン | z ズーム | l URL | t/w/n/p タブ | r 再読込 | [ ] 履歴 | a バッジ | m ピン | s ピン保存 | q 終了",
            "1/2/4 布局 | 方向键 窗格 | z 缩放 | l 网址 | t/w/n/p 标签页 | r 刷新 | [ ] 历史 | a 标记 | m 图钉 | s 保存图钉 | q 退出",
            "1/2/4 diseño | flechas panel | z zoom | l url | t/w/n/p pestañas | r recargar | [ ] historial | a insignias | m pin | s guardar pines | q salir",
            "1/2/4 disposition | flèches panneau | z zoom | l url | t/w/n/p onglets | r recharger | [ ] historique | a badges | m épingle | s enregistrer | q quitter",
            "1/2/4 Layout | Pfeile Bereich | z Zoom | l URL | t/w/n/p Tabs | r Neu laden | [ ] Verlauf | a Marken | m Pin | s Pins speichern | q Beenden",
        ],
        M::Idle => [
            "kb view | Ctrl+{} then ? for keys",
            "kb view | Ctrl+{} 다음 ? = 단축키",
            "kb view | Ctrl+{} の後に ? でキー一覧",
            "kb view | Ctrl+{} 然后按 ? 查看按键",
            "kb view | Ctrl+{} y luego ? para ver las teclas",
            "kb view | Ctrl+{} puis ? pour les touches",
            "kb view | Ctrl+{}, dann ? für Tasten",
        ],
        M::PromptOpen => ["open: ", "주소: ", "開く: ", "打开: ", "abrir: ", "ouvrir : ", "öffnen: "],
        M::PromptPin => [
            "pin #{} on {}: ",
            "핀 #{} ({}): ",
            "ピン #{} ({}): ",
            "图钉 #{} ({}): ",
            "pin #{} en {}: ",
            "épingle #{} sur {} : ",
            "Pin #{} auf {}: ",
        ],
        M::PinMode => [
            "pin: click the element to annotate (Esc cancels)",
            "핀: 메모할 요소를 클릭하세요 (Esc 취소)",
            "ピン: メモを付ける要素をクリック (Esc で取消)",
            "图钉: 点击要标注的元素 (Esc 取消)",
            "pin: haz clic en el elemento a anotar (Esc cancela)",
            "épingle : cliquez sur l'élément à annoter (Échap annule)",
            "Pin: Element zum Kommentieren anklicken (Esc bricht ab)",
        ],
        M::Dialog => [
            "dialog ({})  Enter = accept, Esc = dismiss",
            "대화상자 ({})  Enter = 확인, Esc = 닫기",
            "ダイアログ ({})  Enter = OK, Esc = 閉じる",
            "对话框 ({})  Enter = 确定, Esc = 关闭",
            "diálogo ({})  Enter = aceptar, Esc = descartar",
            "dialogue ({})  Entrée = accepter, Échap = fermer",
            "Dialog ({})  Enter = annehmen, Esc = schließen",
        ],
        M::Opening => [
            "opening {} ...",
            "{} 여는 중 ...",
            "{} を開いています ...",
            "正在打开 {} ...",
            "abriendo {} ...",
            "ouverture de {} ...",
            "öffne {} ...",
        ],
        M::SavedPins => [
            "saved {} pins: kb-pins.md, kb-pins.json in {}",
            "핀 {}개 저장: kb-pins.md, kb-pins.json ({})",
            "ピン {} 件を保存: kb-pins.md, kb-pins.json ({})",
            "已保存 {} 个图钉: kb-pins.md, kb-pins.json ({})",
            "{} pines guardados: kb-pins.md, kb-pins.json en {}",
            "{} épingles enregistrées : kb-pins.md, kb-pins.json dans {}",
            "{} Pins gespeichert: kb-pins.md, kb-pins.json in {}",
        ],
        M::CannotWritePins => [
            "error: cannot write pins: {}",
            "오류: 핀을 저장할 수 없음: {}",
            "エラー: ピンを保存できません: {}",
            "错误: 无法保存图钉: {}",
            "error: no se pueden guardar los pines: {}",
            "erreur : impossible d'enregistrer les épingles : {}",
            "Fehler: Pins können nicht gespeichert werden: {}",
        ],
        M::NoteHint => [
            "type a note, Enter saves, Esc cancels",
            "메모 입력, Enter 저장, Esc 취소",
            "メモを入力、Enter で保存、Esc で取消",
            "输入备注，Enter 保存，Esc 取消",
            "escribe una nota, Enter guarda, Esc cancela",
            "saisissez une note, Entrée enregistre, Échap annule",
            "Notiz eingeben, Enter speichert, Esc bricht ab",
        ],
    }
}

pub fn t(m: M) -> &'static str {
    table(m)[lang() as usize]
}

/// `t` with each `{}` replaced by the next argument.
pub fn f(m: M, args: &[&str]) -> String {
    fill(t(m), args)
}

fn fill(pattern: &str, args: &[&str]) -> String {
    let mut out = String::new();
    let mut parts = pattern.split("{}");
    out.push_str(parts.next().unwrap_or(""));
    for (i, p) in parts.enumerate() {
        out.push_str(args.get(i).copied().unwrap_or(""));
        out.push_str(p);
    }
    out
}

/// What an error code means, for people reading in another language. The
/// `error[code]: message` line itself stays English. None in English.
pub fn explain(code: &str) -> Option<&'static str> {
    let row: [&'static str; 7] = match code {
        "usage" => [
            "",
            "명령 사용법이 틀렸습니다. kb help 를 보세요.",
            "コマンドの使い方が違います。kb help を見てください。",
            "命令用法有误。请查看 kb help。",
            "uso incorrecto del comando. Consulta kb help.",
            "commande mal utilisée. Voir kb help.",
            "Befehl falsch verwendet. Siehe kb help.",
        ],
        "stale_ref" => [
            "",
            "ref 가 더 이상 유효하지 않습니다 (요소가 사라졌거나 페이지가 이동함). kb snapshot 을 다시 실행하세요.",
            "ref が無効です（要素が消えたか、ページが移動しました）。kb snapshot をやり直してください。",
            "ref 已失效（元素已消失或页面已跳转）。请重新运行 kb snapshot。",
            "la ref ya no es válida (el elemento desapareció o la página navegó). Ejecuta kb snapshot otra vez.",
            "la ref n'est plus valide (élément disparu ou page changée). Relancez kb snapshot.",
            "die Ref ist nicht mehr gültig (Element entfernt oder Seite gewechselt). kb snapshot erneut ausführen.",
        ],
        "timeout" => [
            "",
            "시간 안에 끝나지 않았습니다. --timeout 으로 늘릴 수 있습니다.",
            "時間内に終わりませんでした。--timeout で延長できます。",
            "未在限定时间内完成。可用 --timeout 延长。",
            "no terminó a tiempo. Puedes ampliar el plazo con --timeout.",
            "délai dépassé. Augmentez-le avec --timeout.",
            "nicht rechtzeitig fertig. Mit --timeout verlängern.",
        ],
        "not_actionable" => [
            "",
            "요소를 조작할 수 없습니다 (가려짐, 숨김, 비활성 등).",
            "要素を操作できません（隠れている、非表示、無効など）。",
            "无法操作该元素（被遮挡、隐藏或禁用等）。",
            "no se puede actuar sobre el elemento (tapado, oculto, deshabilitado...).",
            "impossible d'agir sur l'élément (couvert, masqué, désactivé...).",
            "Element nicht bedienbar (verdeckt, versteckt, deaktiviert ...).",
        ],
        "no_such_tab" => [
            "",
            "그런 탭이 없습니다. kb tabs 로 확인하세요.",
            "そのタブはありません。kb tabs で確認してください。",
            "没有这个标签页。请用 kb tabs 查看。",
            "no existe esa pestaña. Consulta kb tabs.",
            "cet onglet n'existe pas. Voir kb tabs.",
            "diesen Tab gibt es nicht. Siehe kb tabs.",
        ],
        "eval_error" => [
            "",
            "페이지의 JavaScript 가 예외를 던졌습니다.",
            "ページの JavaScript が例外を投げました。",
            "页面中的 JavaScript 抛出了异常。",
            "el JavaScript de la página lanzó una excepción.",
            "le JavaScript de la page a levé une exception.",
            "das JavaScript der Seite hat eine Ausnahme ausgelöst.",
        ],
        "dialog" => [
            "",
            "페이지에 대화상자가 열려 있습니다. kb dialog accept 또는 dismiss 로 닫으세요.",
            "ページにダイアログが開いています。kb dialog accept か dismiss で閉じてください。",
            "页面上有打开的对话框。请用 kb dialog accept 或 dismiss 关闭。",
            "hay un diálogo abierto en la página. Ciérralo con kb dialog accept o dismiss.",
            "un dialogue est ouvert sur la page. Fermez-le avec kb dialog accept ou dismiss.",
            "auf der Seite ist ein Dialog offen. Mit kb dialog accept oder dismiss schließen.",
        ],
        "terminal" => [
            "",
            "터미널 문제입니다. kb view --probe 로 지원 기능을 확인하세요.",
            "端末の問題です。kb view --probe で対応機能を確認してください。",
            "终端问题。请用 kb view --probe 检查支持的功能。",
            "problema del terminal. Comprueba sus capacidades con kb view --probe.",
            "problème de terminal. Vérifiez ses capacités avec kb view --probe.",
            "Terminalproblem. Fähigkeiten mit kb view --probe prüfen.",
        ],
        // "daemon", "chrome" and anything unknown.
        _ => [
            "",
            "데몬 또는 Chrome 문제입니다. 다시 시도하거나 kb stop 후 재실행하세요.",
            "デーモンまたは Chrome の問題です。再試行するか、kb stop の後にやり直してください。",
            "守护进程或 Chrome 出现问题。请重试，或先 kb stop 再运行。",
            "problema del daemon o de Chrome. Reintenta, o ejecuta kb stop y vuelve a empezar.",
            "problème du démon ou de Chrome. Réessayez, ou faites kb stop puis relancez.",
            "Problem mit dem Daemon oder Chrome. Erneut versuchen oder kb stop und neu starten.",
        ],
    };
    Some(row[lang() as usize]).filter(|s| !s.is_empty())
}

/// `kb help` in the current language; None = use the English text.
pub fn help() -> Option<&'static str> {
    match lang() {
        Lang::En => None,
        Lang::Ko => Some(HELP_KO),
        Lang::Ja => Some(HELP_JA),
        Lang::Zh => Some(HELP_ZH),
        Lang::Es => Some(HELP_ES),
        Lang::Fr => Some(HELP_FR),
        Lang::De => Some(HELP_DE),
    }
}

const HELP_KO: &str = r#"kb - AI 에이전트와 사람이 함께 쓰는 터미널 브라우저 (CDP 로 Chrome 제어)

사용법  kb <명령> [인자] [--tab <id>] [--json] [--timeout <ms>]
        첫 실행 때 백그라운드 데몬과 headless Chrome 이 뜹니다.

페이지
  open <url>               이동하고 로드가 끝날 때까지 대기
  snapshot                 접근성 트리. 조작 가능한 요소에 @N ref 부여
      -i                   조작 가능한 요소만
      --scope <ref>        해당 ref 아래만
      --max-nodes <n>      출력 상한 (기본 2000, 잘리면 표시됨)
  text [ref]               페이지 또는 요소 하나의 보이는 글자
  eval <js> | --stdin      JavaScript 실행, 결과 출력
  screenshot <file.png>    뷰포트를 PNG 로 저장
  wait <text|ref>          글자/요소가 나타날 때까지 대기 (--gone: 사라질 때까지,
                           --text: 숫자 인자도 글자로 취급)
  back | forward | reload

입력 (브라우저의 진짜 trusted 이벤트)
  click <ref>              화면 안으로 스크롤. 요소가 가려져 있으면 실패
  fill <ref> <text>        텍스트 칸, <select> (값 또는 라벨), 체크박스
                           (true/false), 파일 입력 (경로). 글자는 --stdin 가능
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) 끝난 뒤 새 스냅샷을 덧붙임
  dialog accept [text] | dialog dismiss      alert/confirm/prompt 응답

탭
  tabs                     탭 목록 (* = 현재)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               현재 탭을 바꾸지 않고 특정 탭을 지정

배지와 핀 (`kb view` 의 사람과 에이전트가 공유)
  badges [on|off|toggle]   각 ref 의 @N 을 페이지 위에 그림. 뷰어와
                           kb screenshot 에도 보임
  pins                     요소에 붙인 메모를 마크다운으로 (--json)
      --save <file>        파일로 저장: .json 이면 JSON, 그 외 마크다운
  pin add <ref> <note>     요소에 메모 고정
  pin note <N> <text>      핀 #N 메모 수정      pin rm <N>     pins clear
      핀은 페이지 URL, 셀렉터, role/name, 글자, 위치를 기록합니다. 요소가
      열린 페이지에 있는 동안은 살아 있는 ref 를 갖고, 아니면 detached 로
      표시됩니다.

뷰어 (사람용, 실시간. kitty graphics 필요: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               탭을 터미널에 스트리밍. 마우스와 키는 페이지로
                           전달됩니다. 위 명령들과 같은 탭을 씁니다.
      --layout 1|2|4       탭 하나, 1x2, 2x2
      --zoom fit|1280|1920|2560   레이아웃 뷰포트 너비 (CSS zoom 아님)
      --probe              이 터미널이 지원하는 기능을 보고하고 종료
      --force              그래픽 지원 확인을 건너뜀
      --keys               키/마우스 동작이 무엇으로 도착하는지 출력, q q q 로 종료
      --leader <letter>    리더는 Ctrl+<letter> (기본: Ctrl+G 또는 Ctrl+B)
      키: 리더 다음에 1/2/4 분할, 화살표 창 이동, z 줌, l 주소, t 새 탭,
          w 탭 닫기, n/p 다음/이전 탭, r 새로고침, [ ] 뒤/앞, q 종료,
          a @N 배지 토글, m 핀, s 핀 저장 (kb-pins.md + .json), ? 도움말
      요소에 메모 고정: 우클릭, 또는 Alt+클릭, 또는 리더 m 다음 클릭
      (Enter 저장, Esc 취소, 빈 메모는 삭제).
      Ctrl+우클릭은 진짜 우클릭을 보냅니다.
      tmux, Herdr 등 안에서는 Ctrl+B 와 우클릭을 멀티플렉서가 가져가는
      경우가 많습니다: Ctrl+G 와 리더 m 을 쓰세요.

데몬
  status | stop            첫 명령에 --headed 를 주면 창이 보입니다

언어
  lang [code|auto]         언어를 보거나 정함 (저장됨): en ko ja zh es fr de
  --lang <code>            이번 실행만. 환경변수 KB_LANG 도 같음.
                           명령 출력, JSON, 오류 코드는 항상 영어입니다.

REF   ref 는 숫자만 쓰세요: kb click 3   (@3, e3 도 되지만 PowerShell 은
      맨 @3 을 삼킵니다). ref 는 요소가 존재하는 동안 번호가 유지됩니다.
      요소가 제거되거나 페이지가 이동하면 stale 이 됩니다. 그때는
      kb snapshot 을 다시 실행하세요.

종료코드  0 성공 | 1 사용법 | 2 stale ref | 3 데몬/Chrome | 4 시간 초과
          5 조작 불가 (가려짐, 숨김, 비활성) | 6 없는 탭
          7 JavaScript 예외 | 8 대화상자 열림

환경변수  KB_HOME 상태 폴더 | KB_CHROME 브라우저 실행 파일 | KB_HEADED=1 | KB_LANG
"#;

const HELP_JA: &str = r#"kb - AI エージェントと人が共用する端末ブラウザ (CDP で Chrome を操作)

使い方  kb <コマンド> [引数] [--tab <id>] [--json] [--timeout <ms>]
        初回実行時にバックグラウンドのデーモンと headless Chrome が起動します。

ページ
  open <url>               移動して読み込み完了まで待つ
  snapshot                 アクセシビリティツリー。操作できる要素に @N ref を付与
      -i                   操作できる要素のみ
      --scope <ref>        指定 ref の配下のみ
      --max-nodes <n>      出力の上限 (既定 2000、切り詰めは明示)
  text [ref]               ページまたは要素 1 つの表示テキスト
  eval <js> | --stdin      JavaScript を実行し結果を表示
  screenshot <file.png>    ビューポートを PNG で保存
  wait <text|ref>          テキスト/要素が現れるまで待つ (--gone: 消えるまで、
                           --text: 数値の引数もテキストとして扱う)
  back | forward | reload

入力 (ブラウザの本物の trusted イベント)
  click <ref>              画面内へスクロール。要素が覆われていれば失敗
  fill <ref> <text>        テキスト欄、<select> (値またはラベル)、チェックボックス
                           (true/false)、ファイル入力 (パス)。テキストは --stdin 可
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) 終了後に新しいスナップショットを付加
  dialog accept [text] | dialog dismiss      alert/confirm/prompt に応答

タブ
  tabs                     タブ一覧 (* = 現在)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               現在のタブを変えずに対象タブを指定

バッジとピン (`kb view` の人とエージェントで共有)
  badges [on|off|toggle]   各 ref の @N をページ上に描画。ビューアと
                           kb screenshot にも映ります
  pins                     要素に付けたメモを Markdown で表示 (--json)
      --save <file>        ファイルに保存: .json なら JSON、それ以外は Markdown
  pin add <ref> <note>     要素にメモをピン留め
  pin note <N> <text>      ピン #N のメモを変更      pin rm <N>     pins clear
      ピンはページ URL、セレクタ、role/name、テキスト、位置を記録します。
      要素が開いているページにある間は有効な ref を持ち、そうでなければ
      detached と表示されます。

ビューア (人向け、ライブ。kitty graphics が必要: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               タブを端末にストリーミング。マウスとキーはページへ
                           送られます。上のコマンドと同じタブを使います。
      --layout 1|2|4       タブ 1 つ、1x2、2x2
      --zoom fit|1280|1920|2560   レイアウトビューポートの幅 (CSS zoom ではない)
      --probe              この端末の対応機能を報告して終了
      --force              グラフィックス対応の確認を省略
      --keys               キー/マウス操作がどう届くかを表示、q q q で終了
      --leader <letter>    リーダーは Ctrl+<letter> (既定: Ctrl+G または Ctrl+B)
      キー: リーダーの後に 1/2/4 分割、矢印 ペイン移動、z ズーム、l URL、
            t 新規タブ、w タブを閉じる、n/p 次/前のタブ、r 再読込、[ ] 履歴、
            q 終了、a @N バッジ切替、m ピン、s ピン保存 (kb-pins.md + .json)、
            ? ヘルプ
      要素にメモをピン留め: 右クリック、Alt+クリック、またはリーダー m の後に
      クリック (Enter で保存、Esc で取消、空のメモは削除)。
      Ctrl+右クリックは本物の右クリックを送ります。
      tmux や Herdr などの中では Ctrl+B と右ボタンはマルチプレクサが使うことが
      多いので、Ctrl+G とリーダー m を使ってください。

デーモン
  status | stop            最初のコマンドに --headed を付けるとウィンドウを表示

言語
  lang [code|auto]         言語の表示/設定 (保存されます): en ko ja zh es fr de
  --lang <code>            今回の実行のみ。環境変数 KB_LANG も同じ。
                           コマンド出力、JSON、エラーコードは常に英語です。

REF   ref は数字だけで書きます: kb click 3   (@3 や e3 も使えますが、PowerShell は
      素の @3 を飲み込みます)。ref は要素が存在する間は同じ番号です。要素が
      削除されるかページが移動すると stale になります。その場合は
      kb snapshot をやり直してください。

終了コード  0 成功 | 1 使い方 | 2 stale ref | 3 デーモン/Chrome | 4 タイムアウト
            5 操作不可 (覆われている、非表示、無効) | 6 タブなし
            7 JavaScript 例外 | 8 ダイアログ表示中

環境変数  KB_HOME 状態ディレクトリ | KB_CHROME ブラウザ実行ファイル | KB_HEADED=1 | KB_LANG
"#;

const HELP_ZH: &str = r#"kb - 供 AI 代理和人共用的终端浏览器 (通过 CDP 控制 Chrome)

用法  kb <命令> [参数] [--tab <id>] [--json] [--timeout <ms>]
      首次使用时会启动后台守护进程和 headless Chrome。

页面
  open <url>               跳转并等待加载完成
  snapshot                 无障碍树；可交互元素获得 @N ref
      -i                   仅可交互元素
      --scope <ref>        仅某个 ref 之下的子树
      --max-nodes <n>      输出上限 (默认 2000，截断会标明)
  text [ref]               页面或单个元素的可见文本
  eval <js> | --stdin      运行 JavaScript 并输出结果
  screenshot <file.png>    将视口保存为 PNG
  wait <text|ref>          等待文本/元素出现 (--gone: 等待消失，
                           --text: 把数字参数当作文本)
  back | forward | reload

输入 (浏览器的真实 trusted 事件)
  click <ref>              滚动到可见区域；元素被遮挡则失败
  fill <ref> <text>        文本框、<select> (值或标签)、复选框
                           (true/false)、文件输入 (路径)。文本可用 --stdin
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) 结束后附上新的快照
  dialog accept [text] | dialog dismiss      应答 alert/confirm/prompt

标签页
  tabs                     列出标签页 (* = 当前)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               指定标签页而不改变当前标签页

标记与图钉 (`kb view` 中的人与代理共享)
  badges [on|off|toggle]   把每个 ref 的 @N 画在页面上，在查看器和
                           kb screenshot 中都可见
  pins                     钉在元素上的备注，以 Markdown 输出 (--json)
      --save <file>        写入文件: .json 为 JSON，其他为 Markdown
  pin add <ref> <note>     给元素钉上备注
  pin note <N> <text>      修改图钉 #N 的备注      pin rm <N>     pins clear
      图钉记录页面 URL、选择器、role/name、文本和位置。元素在已打开的
      页面上时带有可用的 ref；否则显示为 detached。

查看器 (供人使用，实时；需要 kitty graphics: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               把标签页串流到终端；鼠标和按键发送给页面。
                           与上面的命令共用同一批标签页。
      --layout 1|2|4       单个标签页、1x2 或 2x2
      --zoom fit|1280|1920|2560   布局视口宽度 (不是 CSS zoom)
      --probe              报告此终端支持的功能后退出
      --force              跳过图形能力检查
      --keys               显示每个按键/鼠标操作传来的内容，按 q q q 退出
      --leader <letter>    前导键为 Ctrl+<letter> (默认: Ctrl+G 或 Ctrl+B)
      按键: 前导键之后 1/2/4 布局，方向键 切换窗格，z 缩放，l 网址，t 新标签页，
            w 关闭标签页，n/p 下一个/上一个标签页，r 刷新，[ ] 历史，q 退出，
            a 切换 @N 标记，m 图钉，s 保存图钉 (kb-pins.md + .json)，? 帮助
      给元素钉备注: 右键单击，或 Alt+单击，或前导键 m 之后单击
      (Enter 保存，Esc 取消，空备注即删除)。
      Ctrl+右键 会发送真正的右键单击。
      在 tmux、Herdr 等之中，Ctrl+B 和鼠标右键通常被复用器占用:
      请使用 Ctrl+G 和前导键 m。

守护进程
  status | stop            在第一条命令上加 --headed 可显示窗口

语言
  lang [code|auto]         查看或设置语言 (会保存): en ko ja zh es fr de
  --lang <code>            仅本次运行。环境变量 KB_LANG 作用相同。
                           命令输出、JSON 和错误代码始终为英文。

REF   ref 直接写数字: kb click 3   (@3 和 e3 也可以，但 PowerShell 会吞掉
      单独的 @3)。只要元素存在，ref 的编号就不变；元素被移除或页面跳转后
      它会变为 stale。此时请重新运行 kb snapshot。

退出码  0 成功 | 1 用法 | 2 stale ref | 3 守护进程/Chrome | 4 超时
        5 无法操作 (被遮挡、隐藏、禁用) | 6 没有该标签页
        7 JavaScript 异常 | 8 对话框已打开

环境变量  KB_HOME 状态目录 | KB_CHROME 浏览器可执行文件 | KB_HEADED=1 | KB_LANG
"#;

const HELP_ES: &str = r#"kb - navegador de terminal para agentes de IA y personas (Chrome mediante CDP)

USO    kb <comando> [args] [--tab <id>] [--json] [--timeout <ms>]
       Un daemon en segundo plano y Chrome headless se inician en el primer uso.

PÁGINA
  open <url>               navega y espera la carga
  snapshot                 árbol de accesibilidad; los elementos interactivos reciben refs @N
      -i                   solo elementos interactivos
      --scope <ref>        solo el subárbol bajo una ref
      --max-nodes <n>      límite de salida (2000 por defecto, el recorte se indica)
  text [ref]               texto visible de la página o de un elemento
  eval <js> | --stdin      ejecuta JavaScript e imprime el resultado
  screenshot <file.png>    guarda un PNG del viewport
  wait <text|ref>          espera a que aparezca el texto/elemento (--gone: a que
                           desaparezca, --text: trata un argumento numérico como texto)
  back | forward | reload

ENTRADA (eventos reales y trusted del navegador)
  click <ref>              desplaza hasta hacerlo visible; falla si el elemento está tapado
  fill <ref> <text>        campos de texto, <select> (valor o etiqueta), casilla
                           (true/false), entradas de archivo (ruta). --stdin para texto
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) añade un snapshot nuevo al final
  dialog accept [text] | dialog dismiss      responde a alert/confirm/prompt

PESTAÑAS
  tabs                     lista las pestañas (* = actual)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               apunta a una pestaña sin cambiar la actual

INSIGNIAS Y PINES (compartidos por la persona en `kb view` y el agente)
  badges [on|off|toggle]   dibuja el @N de cada ref en la propia página; se ve
                           en el visor y en kb screenshot
  pins                     notas fijadas a elementos, en markdown (--json)
      --save <file>        las escribe: .json da JSON, cualquier otro markdown
  pin add <ref> <note>     fija una nota a un elemento
  pin note <N> <text>      cambia la nota del pin #N      pin rm <N>     pins clear
      Un pin registra URL de la página, selector, role/name, texto y caja.
      Lleva una ref viva mientras su elemento esté en una página abierta;
      si no, aparece como detached.

VISOR (para personas, en vivo; requiere kitty graphics: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               transmite pestañas al terminal; ratón y teclas van
                           a la página. Comparte pestañas con los comandos de arriba.
      --layout 1|2|4       una pestaña, 1x2 o 2x2
      --zoom fit|1280|1920|2560   ancho del viewport de diseño (no es CSS zoom)
      --probe              informa de lo que admite este terminal y sale
      --force              omite la comprobación de gráficos
      --keys               muestra lo que entrega cada tecla/acción del ratón; q q q sale
      --leader <letter>    la tecla líder es Ctrl+<letter> (por defecto: Ctrl+G o Ctrl+B)
      Teclas: líder y luego 1/2/4 diseño, flechas panel, z zoom, l url, t pestaña nueva,
              w cerrar pestaña, n/p pestaña siguiente/anterior, r recargar, [ ] historial,
              q salir, a alternar insignias @N, m pin, s guardar pines
              (kb-pins.md + .json), ? ayuda
      Fijar una nota a un elemento: clic derecho, o Alt+clic, o líder m y luego
      clic (Enter guarda, Esc cancela, una nota vacía lo borra).
      Ctrl+clic derecho envía un clic derecho real.
      Dentro de tmux, Herdr y similares, Ctrl+B y el botón derecho suelen
      pertenecer al multiplexor: usa Ctrl+G y líder m.

DAEMON
  status | stop            --headed en el primer comando muestra la ventana

IDIOMA
  lang [code|auto]         muestra o fija el idioma (se recuerda): en ko ja zh es fr de
  --lang <code>            solo para esta ejecución. KB_LANG hace lo mismo.
                           La salida de comandos, el JSON y los códigos de error
                           están siempre en inglés.

REFS  Escribe las refs como números: kb click 3   (@3 y e3 también valen, pero
      PowerShell se traga un @3 suelto). Una ref conserva su número mientras
      exista su elemento; queda stale cuando el elemento se elimina o la
      página navega. Entonces ejecuta kb snapshot de nuevo.

SALIDA  0 ok | 1 uso | 2 stale ref | 3 daemon/Chrome | 4 tiempo agotado
        5 no accionable (tapado, oculto, deshabilitado) | 6 no existe la pestaña
        7 excepción de JavaScript | 8 diálogo abierto

ENTORNO  KB_HOME directorio de estado | KB_CHROME ejecutable del navegador | KB_HEADED=1 | KB_LANG
"#;

const HELP_FR: &str = r#"kb - navigateur de terminal pour agents IA et humains (Chrome via CDP)

USAGE  kb <commande> [args] [--tab <id>] [--json] [--timeout <ms>]
       Un démon en arrière-plan et Chrome headless démarrent à la première utilisation.

PAGE
  open <url>               navigue et attend le chargement
  snapshot                 arbre d'accessibilité ; les éléments interactifs reçoivent des refs @N
      -i                   éléments interactifs seulement
      --scope <ref>        seulement le sous-arbre sous une ref
      --max-nodes <n>      limite la sortie (2000 par défaut, la troncature est signalée)
  text [ref]               texte visible de la page ou d'un élément
  eval <js> | --stdin      exécute du JavaScript et affiche le résultat
  screenshot <file.png>    enregistre un PNG du viewport
  wait <text|ref>          attend l'apparition du texte/de l'élément (--gone : sa
                           disparition, --text : traite un argument numérique comme du texte)
  back | forward | reload

SAISIE (vrais événements trusted du navigateur)
  click <ref>              fait défiler jusqu'à l'élément ; échoue s'il est recouvert
  fill <ref> <text>        champs de texte, <select> (valeur ou libellé), case à cocher
                           (true/false), champs de fichier (chemin). --stdin pour le texte
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) ajoute un nouveau snapshot à la fin
  dialog accept [text] | dialog dismiss      répond à alert/confirm/prompt

ONGLETS
  tabs                     liste les onglets (* = courant)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               cible un onglet sans changer l'onglet courant

BADGES ET ÉPINGLES (partagés entre l'humain dans `kb view` et l'agent)
  badges [on|off|toggle]   dessine le @N de chaque ref sur la page elle-même ;
                           visible dans la visionneuse et dans kb screenshot
  pins                     notes épinglées aux éléments, en markdown (--json)
      --save <file>        les écrit : .json donne du JSON, sinon du markdown
  pin add <ref> <note>     épingle une note à un élément
  pin note <N> <text>      modifie la note de l'épingle #N      pin rm <N>     pins clear
      Une épingle enregistre l'URL de la page, le sélecteur, role/name, le
      texte et la boîte. Elle porte une ref vivante tant que son élément est
      sur une page ouverte ; sinon elle est indiquée detached.

VISIONNEUSE (pour humains, en direct ; nécessite kitty graphics : Kitty, Ghostty, WezTerm, Herdr)
  view [url]               diffuse les onglets dans le terminal ; souris et touches
                           vont à la page. Partage les onglets avec les commandes ci-dessus.
      --layout 1|2|4       un onglet, 1x2 ou 2x2
      --zoom fit|1280|1920|2560   largeur du viewport de mise en page (pas un CSS zoom)
      --probe              indique ce que ce terminal prend en charge, puis quitte
      --force              ignore la vérification des capacités graphiques
      --keys               affiche ce que livre chaque touche/action de souris ; q q q pour quitter
      --leader <letter>    la touche leader est Ctrl+<letter> (par défaut : Ctrl+G ou Ctrl+B)
      Touches : leader puis 1/2/4 disposition, flèches panneau, z zoom, l url,
                t nouvel onglet, w fermer l'onglet, n/p onglet suivant/précédent,
                r recharger, [ ] historique, q quitter, a badges @N, m épingle,
                s enregistrer les épingles (kb-pins.md + .json), ? aide
      Épingler une note sur un élément : clic droit, ou Alt+clic, ou leader m
      puis clic (Entrée enregistre, Échap annule, une note vide supprime).
      Ctrl+clic droit envoie un vrai clic droit.
      Dans tmux, Herdr et consorts, Ctrl+B et le bouton droit appartiennent
      souvent au multiplexeur : utilisez Ctrl+G et leader m.

DÉMON
  status | stop            --headed sur la première commande affiche la fenêtre

LANGUE
  lang [code|auto]         affiche ou définit la langue (mémorisée) : en ko ja zh es fr de
  --lang <code>            pour cette exécution seulement. KB_LANG fait de même.
                           La sortie des commandes, le JSON et les codes d'erreur
                           restent toujours en anglais.

REFS  Écrivez les refs comme de simples nombres : kb click 3   (@3 et e3
      fonctionnent aussi, mais PowerShell avale un @3 nu). Une ref garde son
      numéro tant que son élément existe ; elle devient stale quand l'élément
      est retiré ou que la page navigue. Relancez alors kb snapshot.

SORTIE  0 ok | 1 usage | 2 stale ref | 3 démon/Chrome | 4 délai dépassé
        5 non actionnable (recouvert, masqué, désactivé) | 6 onglet inexistant
        7 exception JavaScript | 8 dialogue ouvert

ENV   KB_HOME dossier d'état | KB_CHROME exécutable du navigateur | KB_HEADED=1 | KB_LANG
"#;

const HELP_DE: &str = r#"kb - Terminal-Browser für KI-Agenten und Menschen (Chrome über CDP)

AUFRUF  kb <Befehl> [Argumente] [--tab <id>] [--json] [--timeout <ms>]
        Beim ersten Aufruf starten ein Hintergrund-Daemon und headless Chrome.

SEITE
  open <url>               navigiert und wartet auf das Laden
  snapshot                 Accessibility-Baum; interaktive Elemente erhalten @N-Refs
      -i                   nur interaktive Elemente
      --scope <ref>        nur der Teilbaum unter einer Ref
      --max-nodes <n>      Ausgabe begrenzen (Standard 2000, Kürzung wird markiert)
  text [ref]               sichtbarer Text der Seite oder eines Elements
  eval <js> | --stdin      JavaScript ausführen, Ergebnis ausgeben
  screenshot <file.png>    PNG des Viewports speichern
  wait <text|ref>          warten, bis Text/Element erscheint (--gone: verschwindet,
                           --text: numerisches Argument als Text behandeln)
  back | forward | reload

EINGABE (echte, trusted Browser-Ereignisse)
  click <ref>              scrollt ins Bild; schlägt fehl, wenn das Element verdeckt ist
  fill <ref> <text>        Textfelder, <select> (Wert oder Beschriftung), Checkbox
                           (true/false), Dateifelder (Pfad). --stdin für Text
  press <key>              Enter, Tab, Escape, ArrowDown, a, Control+a ...
      --snapshot           (click/fill/press/open) hängt einen frischen Snapshot an
  dialog accept [text] | dialog dismiss      alert/confirm/prompt beantworten

TABS
  tabs                     Tabs auflisten (* = aktuell)
  tab new [url] | tab <id> | tab close [id]
  --tab <id>               einen Tab ansprechen, ohne den aktuellen zu wechseln

MARKEN UND PINS (gemeinsam für den Menschen in `kb view` und den Agenten)
  badges [on|off|toggle]   zeichnet das @N jeder Ref auf die Seite selbst; sichtbar
                           im Viewer und in kb screenshot
  pins                     an Elemente geheftete Notizen als Markdown (--json)
      --save <file>        schreibt sie: .json ergibt JSON, alles andere Markdown
  pin add <ref> <note>     Notiz an ein Element heften
  pin note <N> <text>      Notiz von Pin #N ändern      pin rm <N>     pins clear
      Ein Pin speichert Seiten-URL, Selektor, role/name, Text und Box. Er
      trägt eine gültige Ref, solange sein Element auf einer offenen Seite
      ist; sonst wird er als detached geführt.

VIEWER (für Menschen, live; benötigt kitty graphics: Kitty, Ghostty, WezTerm, Herdr)
  view [url]               streamt Tabs ins Terminal; Maus und Tasten gehen an
                           die Seite. Nutzt dieselben Tabs wie die Befehle oben.
      --layout 1|2|4       ein Tab, 1x2 oder 2x2
      --zoom fit|1280|1920|2560   Breite des Layout-Viewports (kein CSS zoom)
      --probe              meldet, was dieses Terminal unterstützt, und beendet
      --force              Grafikprüfung überspringen
      --keys               zeigt, was jede Taste/Mausaktion liefert; q q q beendet
      --leader <letter>    Leader ist Ctrl+<letter> (Standard: Ctrl+G oder Ctrl+B)
      Tasten: Leader, dann 1/2/4 Layout, Pfeile Bereich, z Zoom, l URL, t neuer Tab,
              w Tab schließen, n/p nächster/vorheriger Tab, r neu laden, [ ] Verlauf,
              q beenden, a @N-Marken umschalten, m Pin, s Pins speichern
              (kb-pins.md + .json), ? Hilfe
      Notiz an ein Element heften: Rechtsklick, oder Alt+Klick, oder Leader m
      und dann Klick (Enter speichert, Esc bricht ab, leere Notiz löscht).
      Ctrl+Rechtsklick sendet einen echten Rechtsklick.
      In tmux, Herdr und Ähnlichem gehören Ctrl+B und die rechte Maustaste
      meist dem Multiplexer: Ctrl+G und Leader m verwenden.

DAEMON
  status | stop            --headed beim ersten Befehl zeigt das Fenster

SPRACHE
  lang [code|auto]         Sprache anzeigen oder festlegen (wird gemerkt): en ko ja zh es fr de
  --lang <code>            nur für diesen Aufruf. KB_LANG wirkt genauso.
                           Befehlsausgabe, JSON und Fehlercodes sind immer englisch.

REFS  Refs als einfache Zahlen schreiben: kb click 3   (@3 und e3 gehen auch,
      aber PowerShell verschluckt ein bloßes @3). Eine Ref behält ihre Nummer,
      solange ihr Element existiert; sie wird stale, wenn das Element entfernt
      wird oder die Seite navigiert. Dann kb snapshot erneut ausführen.

EXIT  0 ok | 1 Aufruf | 2 stale ref | 3 Daemon/Chrome | 4 Zeitüberschreitung
      5 nicht bedienbar (verdeckt, versteckt, deaktiviert) | 6 kein solcher Tab
      7 JavaScript-Ausnahme | 8 Dialog offen

UMGEBUNG  KB_HOME Zustandsordner | KB_CHROME Browser-Programm | KB_HEADED=1 | KB_LANG
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_names() {
        assert_eq!(parse("ko"), Some(Lang::Ko));
        assert_eq!(parse("ko_KR.UTF-8"), Some(Lang::Ko));
        assert_eq!(parse("ko-KR"), Some(Lang::Ko));
        assert_eq!(parse("zh_CN"), Some(Lang::Zh));
        assert_eq!(parse("Japanese_Japan.932"), Some(Lang::Ja));
        assert_eq!(parse("DE"), Some(Lang::De));
        assert_eq!(parse("C"), None);
        assert_eq!(parse("POSIX"), None);
        assert_eq!(parse(""), None);
        assert!(valid("auto") && valid("fr") && !valid("xx"));
    }

    #[test]
    fn every_message_is_translated_with_the_same_placeholders() {
        for m in KEYS {
            let row = table(m);
            let holes = row[0].matches("{}").count();
            for (i, s) in row.iter().enumerate() {
                assert!(!s.trim().is_empty(), "{:?} is empty in {}", m, CODES[i]);
                assert_eq!(s.matches("{}").count(), holes, "{:?} in {} has different placeholders", m, CODES[i]);
                assert!(!s.contains('\n'), "{:?} in {} must be one line", m, CODES[i]);
            }
        }
    }

    #[test]
    fn help_texts_cover_every_command() {
        for (name, text) in [("ko", HELP_KO), ("ja", HELP_JA), ("zh", HELP_ZH), ("es", HELP_ES), ("fr", HELP_FR), ("de", HELP_DE)] {
            for word in [
                "open <url>", "snapshot", "--scope <ref>", "--max-nodes <n>", "text [ref]", "eval <js>", "screenshot <file.png>",
                "wait <text|ref>", "back | forward | reload", "click <ref>", "fill <ref> <text>", "press <key>", "dialog accept",
                "tab new [url]", "badges [on|off|toggle]", "pin add <ref> <note>", "pins clear", "view [url]", "--layout 1|2|4",
                "--zoom fit|1280|1920|2560", "--probe", "--force", "--keys", "--leader <letter>", "status | stop",
                "lang [code|auto]", "--lang <code>", "KB_HOME", "KB_CHROME", "KB_LANG", "kb click 3",
            ] {
                assert!(text.contains(word), "{} help lacks {:?}", name, word);
            }
        }
    }

    #[test]
    fn placeholders_fill_in_order() {
        assert_eq!(fill("pin #{} on {}: ", &["3", "button"]), "pin #3 on button: ");
        assert_eq!(fill("{} 여는 중", &["x"]), "x 여는 중");
        assert_eq!(fill("none", &["x"]), "none");
    }
}
