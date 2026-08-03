# What a real user should experience

Five ordinary mistakes, one after another, in the file the user is editing by
hand. Every one of them has the same requirement: the tool must blame the spec,
not the board.

## Why exit 2 is the whole scenario

The exit code is the contract a pipeline reads:

- **0** would mean the board passed. It did not, because nothing was checked.
- **1** would mean the board failed, and would send someone to the bench with a
  scope to chase a fault that is a typo in a TOML file.
- **2** means the spec or the board input is wrong. That is the truth, and it is
  the only answer that puts the reader in the right file.

So every step here asserts exit 2 and asserts the absence of `[FAIL]`, `RED`,
and `assertions passed`. No assertion was evaluated, so no assertion may be
reported.

## The five mistakes

**min above max.** `min = 3.6, max = 3.0` is a window nothing can satisfy. This
one is worth catching specially because it *would* run: it would just always
fail, and the user would spend an afternoon believing their rail is broken. The
message names both numbers back and suggests the two likely fixes.

**A supply with no voltage.** The tool could guess 3.3 V from the net name. It
must not, and it says why: a wrong guess fabricates faults on a healthy board,
which costs more trust than an error message costs patience.

**A misspelled kind.** `voltag` gets a did-you-mean plus the full list of kinds
that exist, so the answer is in the error rather than in the documentation.

**A net that is not on the board.** `+3V4` gets the nearest real names and the
section of the spec that mentioned it. The section matters once a spec is long
enough to name the same net in three places.

**TOML that does not parse.** A line number and the parser's own complaint. Not
a panic, not a backtrace: a malformed config file is the most ordinary thing a
user can hand a tool, and a stack trace for it reads as "this tool is broken"
rather than "your file has a typo on line 1".
