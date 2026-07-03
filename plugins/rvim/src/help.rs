//! Content of the `:help` buffer.

/// Keybinding cheatsheet shown by `:help`.
pub const HELP_TEXT: &str = r#"rvim — cheatsheet
=================

MODES
  i I a A o O        enter insert (before/first-nonblank/after/eol/line below/above)
  jk                 quick-escape: typing j then k leaves insert mode
  Esc  Ctrl+c        back to normal mode
  v / V              visual charwise / linewise (o swaps ends)
  R                  replace mode (overtype)
  :                  ex command line     / ?  search

MOTIONS   (all take a count, e.g. 5j, 3w, d2w)
  h j k l  arrows    left / down / up / right
  w W  b B  e E  ge  word fwd / back / end / prev-end (W B E = WORD)
  0 ^ $  Home End    line start / first non-blank / line end
  gg  G  {n}G  :{n}  first line / last line / line n
  { }                previous / next blank-line paragraph
  %                  matching bracket () [] {}
  f{c} F{c} t{c} T{c}  find char on line; ; repeats , reverses
  ` {m}  ' {m}       jump to mark (exact / line start)

OPERATORS   (double for whole lines: dd yy cc >> << gcc)
  d c y              delete / change / yank        x X s S C D Y  shorthands
  > <                indent / dedent by 4 spaces
  gu gU              lowercase / uppercase         ~  toggle case
  gc  gcc            toggle // line comment
  r{c}  R            replace char(s) / overtype    J  join lines with a space

TEXT OBJECTS   (after d/c/y or in visual)
  iw aw  iW aW       inner / around word (WORD)
  i( a(  i{ a{  i[ a[  i< a<     brackets (also ib ab / iB aB)
  i" a"  i' a'  i` a`            quoted strings (same line)

REGISTERS & MARKS
  "{a-z}             pick a register for the next d/c/y/p
  ""                 unnamed register    "0  last yank
  p P                paste after / before (linewise pastes open lines)
  m{a-z}             set mark            `a  exact   'a  line

SEARCH   (literal substring — no regex!)
  / ?                search forward / backward (incremental)
  n N                next / previous match (wraps with a message)
  * #                search the word under the cursor
  :noh               hide match highlighting until the next search

UNDO & REPEAT
  u  Ctrl+r          undo / redo (one insert session = one undo)
  .                  repeat the last change (incl. typed insert text)

SCROLLING
  Ctrl+d Ctrl+u      half page down / up
  Ctrl+f Ctrl+b      page down / up (also PageDown / PageUp)
  zz zt zb           center / top / bottom the current line

FILES & BUFFERS
  :w [name]  :wa     write buffer (or copy to name) / write all
  :e name  :e!       open or create / reload from disk
  :enew              new empty buffer
  :ls  :b {n|name}   list buffers / switch    :bn :bp  next / prev
  :q  :q!  :wq  :x   close (! discards changes)
  :bd[!]             delete buffer            :rm name  delete file
  Ctrl+p  Space f f  fuzzy file finder (type to filter, Enter opens,
                     Enter with no match creates the file)

COMMANDS
  :{n}  :$           jump to line n / last line
  :[range]s/pat/rep/[g]   substitute, literal pat; ranges % N,M . $
  :set nu nonu rnu nornu  line numbers absolute / relative
  :reg               show registers      :help  this page

TOUCHBAR (bottom row)
  esc ctrl tab : /   taps send keys; ctrl stays armed for the next key
  ← ↓ ↑ →            arrows              ⌨  show / hide the keyboard
  tap the text to place the cursor; drag to scroll
"#;

/// Buffer name the help text opens under.
pub const HELP_BUFFER: &str = "[help]";
