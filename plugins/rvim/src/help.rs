//! Content of the `:help` buffer.

/// Keybinding cheatsheet shown by `:help`.
pub const HELP_TEXT: &str = r#"rvim — cheatsheet
=================

MODES
  i I a A o O        enter insert (before/first-nonblank/after/eol/line below/above)
  jk                 quick-escape: typing j then k leaves insert mode
  Esc  Ctrl+c        back to normal mode
  v / V / Ctrl+v     visual charwise / linewise / blockwise (o swaps ends)
  R                  replace mode (overtype)
  :                  ex command line     / ?  search

INSERT MODE EDITS
  Ctrl+w  Alt+Backspace   delete the word before the cursor
  Ctrl+u             delete from the line start to the cursor
  Tab                insert 4 spaces     Enter  keeps the indent

LEADER (Space; a which-key panel above the touchbar lists these)
  Space e            toggle the file explorer (opening focuses it)
  Space f f          find file (fuzzy)   Space f b  pick an open buffer
  Space o            focus explorer <-> editor, or next window
  Space t h/v        split below / right Space t q  close window
  Space w            save (:w)           Space c    close buffer (:bd)
  Space q            close window (:q when it is the last one)
  Space h            open this help
  note: pausing on any prefix — g z " m ` ' f r q @ or an operator
        (d c y > < …) — shows the same panel for that prefix's keys;
        tap a hint to press it, so no combo has to be memorized

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

VISUAL BLOCK (Ctrl+v)
  motions            size the rectangle  o / O  jump between corners
  d x  y  c          delete / yank / change the block (yanks paste back as a block)
  I  A               insert left / right of the block on every line (Esc applies)
  $  A               append at each line's end      r{c}  fill the block with c
  D  C               delete / change from the block edge to the line ends
  > <                indent the spanned lines

MACROS
  q{a-z}             record keys into a macro; q again stops (q{A-Z} appends)
  @{a-z}  @@         replay a macro / the last one; takes a count, e.g. 5@a
                     the statusline shows "recording @x" while one is armed

REGISTERS & MARKS
  "{a-z}             pick a register for the next d/c/y/p
  ""                 unnamed register    "0  last yank
  p P                paste after / before (linewise opens lines, block pastes a rect)
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

WINDOWS (splits)
  Ctrl+w s / v       split below / right      Ctrl+w w  next window
  Ctrl+w h j k l     previous / next window (arrows too)
  Ctrl+w q / c       close window
  :sp :split         split below              :vs :vsplit  split right
  :close             close window             :only  keep only this one
  :q                 closes the window first when several are open
  note: two windows on the SAME buffer share one cursor and scroll

FILE EXPLORER (Space e)
  j k  Up Down       move        g / G  first / last file
  Enter  l           open the selected file
  h  Esc             back to the editor (sidebar stays)
  a                  new file: type a name, Enter creates+opens, Esc cancels
  d d                delete the selected file (refused while a modified
                     buffer holds it; a clean buffer is closed too)
  q                  hide the explorer         tap rows to select / open

FILES & BUFFERS
  :w [name]  :wa     write buffer (or copy to name) / write all
  :e name  :e!       open or create / reload from disk
  :enew              new empty buffer
  :ls  :b {n|name}   list buffers / switch    :bn :bp  next / prev
  :q  :q!  :wq  :x   close (! discards changes)
  :bd[!]             delete buffer            :rm name  delete file
  Ctrl+p  Space f f  fuzzy file finder (type to filter, Enter opens,
                     Enter with no match creates the file)
  Space f b          buffer picker: fuzzy-switch among open buffers

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
