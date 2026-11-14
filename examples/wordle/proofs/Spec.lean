import WordleKernel
/-!
# What Wordle-v0 should do

The file to review: the TextArena marking rule, the score of a set of marks,
and the moves a game has left, on lists and natural numbers with no arrays,
machine words or loops. `Theorems.lean` proves the extracted Rust kernel
agrees with these.
-/
open Aeneas Aeneas.Std wordle_kernel

namespace wordle_kernel.Spec

/-- The letters of a word, as numbers. A letter is below 26; `0` is `a`. -/
def word (w : Array U8 5#usize) : List Nat := w.val.map (·.val)

/-- One copy of `letter` used up. -/
def take (rem : Nat → Nat) (letter : Nat) : Nat → Nat :=
  fun other => if other = letter then rem other - 1 else rem other

/-- Copies of each letter left in the secret after the greens are removed. -/
def afterGreens (secret guess : List Nat) (letter : Nat) : Nat :=
  secret.count letter - ((secret.zip guess).countP (fun p => p.1 = letter ∧ p.2 = letter))

/-- Left to right: a matching position is green; otherwise the letter is
yellow while an unused copy remains, and the copy is used up; otherwise wrong. -/
def markPairs : List (Nat × Nat) → (Nat → Nat) → List Mark
  | [], _ => []
  | (s, g) :: rest, rem =>
    if s = g then .Green :: markPairs rest rem
    else if 0 < rem g then .Yellow :: markPairs rest (take rem g)
    else .Wrong :: markPairs rest rem

/-- The TextArena marking rule: greens first, then yellows from what is left. -/
def mark (secret guess : List Nat) : List Mark :=
  markPairs (secret.zip guess) (afterGreens secret guess)

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
