import WordleKernel
/-!
# What Wordle-v0 should do

The file to review: the score of a set of marks and the moves a game has
left, on lists and natural numbers. `Theorems.lean` proves the extracted Rust
kernel agrees with these.
-/
open Aeneas Aeneas.Std wordle_kernel

namespace wordle_kernel.Spec

def isGreen : Mark → Bool
  | .Green => true
  | _ => false

def isYellow : Mark → Bool
  | .Yellow => true
  | _ => false

/-- `(greens + yellows / 2) / 5` of 1000. -/
def completion (marks : List Mark) : Nat :=
  200 * marks.countP isGreen + 100 * marks.countP isYellow

/-- The number of moves left in a game: two per remaining guess (one
allowed invalid move, then the guess), less any invalid move already spent. -/
def movesLeft (c : Core) : Nat := 2 * (6 - c.guess_count.val) - c.error_count.val

end wordle_kernel.Spec
