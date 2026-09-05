# What a real user should experience

Six things get handed to the tool, none of them a board it can use, and one of
them a board it can use wearing the wrong name. What happens next is most of a
user's first impression, because the first thing anyone does with a new tool is
point it at the wrong file.

## The cases

**An empty file.** "this file is empty", with the filename. Nothing else needs
saying, and nothing else should be said.

**A truncated board.** A line number, a byte offset, and what the parser was
looking for: `parse error at line 18 (byte 400): unclosed list`. Half a board
file is what a crashed export or an interrupted download leaves behind, and the
offset tells the user where it stopped rather than that it stopped.

**A path that does not exist.** The path back, "Check the path", and then a
command they can actually run against a board that ships with the tool. An error
message that leaves the user with nothing to try is a dead end; one that ends in
a runnable line is a tutorial.

**Prose in a .kicad_pcb.** Refused as an unrecognized format, with the list of
formats that would have worked. Not half-parsed into an empty board that then
reports zero findings, which would be the dangerous outcome: a clean bill of
health on a file that is not a board.

**A directory.** Refused with its own message. A folder is a legitimate input
here (gerber output), so the message explains what a gerber folder needs to
contain rather than reporting a filesystem read error.

**A real board with the wrong extension.** This one has to *work*. The extension
is a hint, and the content is the truth; a user who renamed a file, or received
one from a colleague who did, should not be blocked by three characters.

## The rule under all of them

No panic. No backtrace. No `unwrap`, no `Os { code: 2 }`, no doubled article from
a sentence assembled out of two half-templates. A panic on malformed input tells
the user the tool is broken when in fact their file is, and that is an
unrecoverable first impression.
