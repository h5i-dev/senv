import WordleKernel
/-!
# What Wordle-v0 should do

The file to review. Marking and scoring on lists of letter numbers, with no
arrays, machine words or loops. `Theorems.lean` proves the extracted Rust
kernel computes exactly these.

A letter is a number below 26 (`0` is `a`). A word is five of them.
-/
open Aeneas Aeneas.Std wordle_kernel

namespace wordle_kernel.Spec

/-- The letters of a word, as numbers. -/
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

/-- `(greens + yellows / 2) / 5` of 1000. -/
def completion (marks : List Mark) : Nat :=
  200 * marks.count .Green + 100 * marks.count .Yellow

/-- What a valid guess pays: 1000 for the word, else nothing until the last
guess, which pays its completion. -/
def guessReward (secret guess : List Nat) (last : Bool) : Nat :=
  if guess = secret then 1000 else if last then completion (mark secret guess) else 0

/-- The number of moves left in a game: two per remaining guess (one
allowed invalid move, then the guess), less any invalid move already spent. -/
def movesLeft (c : Core) : Nat := 2 * (6 - c.guess_count.val) - c.error_count.val

end wordle_kernel.Spec
