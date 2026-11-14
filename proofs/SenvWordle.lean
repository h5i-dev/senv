/-!
Pure Wordle reset, marking, reward, and termination.

This file follows `mark` and `step_core` in `src/wordle.rs`. It does not model
WebSocket framing, the session table, or the per-session `MemoryEngine`.
Those are shell properties. Nothing here is `sorry` or `admit`.
-/

namespace SenvWordle

inductive Mark where
  | green
  | yellow
  | wrong
  deriving DecidableEq, Repr, BEq

inductive Reason where
  | format
  | length
  | repeat_
  | notEnglish
  deriving DecidableEq, Repr, BEq

/-- Five letters, `0` is `a`. -/
structure Word where
  a : Fin 26
  b : Fin 26
  c : Fin 26
  d : Fin 26
  e : Fin 26
  deriving DecidableEq, Repr

def lettersOf (w : Word) : List (Fin 26) := [w.a, w.b, w.c, w.d, w.e]

def Word.ofNats (a b c d e : Nat)
    (ha : a < 26 := by omega) (hb : b < 26 := by omega)
    (hc : c < 26 := by omega) (hd : d < 26 := by omega)
    (he : e < 26 := by omega) : Word :=
  { a := ⟨a, ha⟩, b := ⟨b, hb⟩, c := ⟨c, hc⟩, d := ⟨d, hd⟩, e := ⟨e, he⟩ }

def bump (rem : Fin 26 → Nat) (letter : Fin 26) : Fin 26 → Nat :=
  fun other => if other = letter then rem other - 1 else rem other

/-- Secret copies left after the green pass. Greens are removed first. -/
def afterGreens (secret guess : Word) (letter : Fin 26) : Nat :=
  (lettersOf secret).count letter -
    ((lettersOf secret).zip (lettersOf guess)).countP
      (fun pair => pair.1 == letter && pair.2 == letter)

def markPairs (pairs : List (Fin 26 × Fin 26)) (rem : Fin 26 → Nat) : List Mark :=
  match pairs with
  | [] => []
  | (s, g) :: rest =>
    if s = g then .green :: markPairs rest rem
    else if rem g > 0 then .yellow :: markPairs rest (bump rem g)
    else .wrong :: markPairs rest rem

/-- Two-pass marker. Same order as Rust `mark`: greens, then yellows. -/
def mark (secret guess : Word) : List Mark :=
  markPairs ((lettersOf secret).zip (lettersOf guess)) (afterGreens secret guess)

def completionMilli (marks : List Mark) : Int :=
  (marks.count .green) * 200 + (marks.count .yellow) * 100

def allGreen (marks : List Mark) : Bool :=
  marks.all (· == .green) && marks.length == 5

structure Core where
  secret : Word
  guesses : List Word
  errorCount : Nat
  turn : Nat
  done : Bool
  won : Bool
  rewardMilli : Int
  deriving DecidableEq, Repr

def fresh (secret : Word) : Core where
  secret := secret
  guesses := []
  errorCount := 0
  turn := 0
  done := false
  won := false
  rewardMilli := 0

def maxGuesses : Nat := 6
def errorAllowance : Nat := 1

inductive PlayOutcome where
  | ongoing
  | win
  | loss (rewardMilli : Int)
  deriving DecidableEq, Repr

inductive Phase where
  | retry (reason : Reason)
  | invalidEnd (reason : Reason) (rewardMilli : Int)
  | played (guessesLeft : Nat) (outcome : PlayOutcome)
  deriving DecidableEq, Repr

/-- A move after the shell has parsed brackets and looked up the dictionary.
`play` means five letters. Repeat and dictionary refusal are decided here,
matching `classify` in Rust. -/
inductive Move where
  | play (guess : Word) (inDictionary : Bool)
  | malformed (reason : Reason)
  deriving DecidableEq, Repr

def latestCompletion (core : Core) : Int :=
  match core.guesses.getLast? with
  | none => 0
  | some guess => completionMilli (mark core.secret guess)

def step (core : Core) (mv : Move) : Option (Core × Phase) :=
  if core.done then none
  else
    match mv with
    | .malformed reason => invalid core reason
    | .play guess inDictionary =>
      if core.guesses.contains guess then invalid core .repeat_
      else if !inDictionary then invalid core .notEnglish
      else if core.guesses.length >= maxGuesses then none
      else
        let marks := mark core.secret guess
        let guessesLeft := maxGuesses - core.turn - 1
        let next : Core :=
          { core with
            guesses := core.guesses ++ [guess]
            errorCount := 0
            turn := core.turn + 1 }
        if allGreen marks then
          some ({ next with done := true, won := true, rewardMilli := 1000 },
            .played guessesLeft .win)
        else if next.guesses.length >= maxGuesses then
          let reward := completionMilli marks
          some ({ next with done := true, won := false, rewardMilli := reward },
            .played guessesLeft (.loss reward))
        else
          some ({ next with done := false, won := false, rewardMilli := 0 },
            .played guessesLeft .ongoing)
where
  invalid (core : Core) (reason : Reason) : Option (Core × Phase) :=
    if core.errorCount < errorAllowance then
      some ({ core with errorCount := core.errorCount + 1, rewardMilli := 0, done := false, won := false },
        .retry reason)
    else
      let reward := latestCompletion core
      some ({ core with done := true, won := false, rewardMilli := reward },
        .invalidEnd reason reward)

def view (result : Option (Core × Phase)) :
    Option (Bool × Bool × Int × Nat × Nat × Nat × Phase) :=
  result.map fun pair =>
    (pair.1.done, pair.1.won, pair.1.rewardMilli, pair.1.turn, pair.1.guesses.length,
      pair.1.errorCount, pair.2)

def playAll (core : Core) : List Word → Option Core
  | [] => some core
  | guess :: rest =>
    match step core (.play guess true) with
    | some (next, _) => playAll next rest
    | none => none

/- Words used by the Rust unit tests. `a` is 0. -/
def crane : Word := .ofNats 2 17 0 13 4
def cigar : Word := .ofNats 2 8 6 0 17
def apple : Word := .ofNats 0 15 15 11 4
def alley : Word := .ofNats 0 11 11 4 24
def llama : Word := .ofNats 11 11 0 12 0
def stone : Word := .ofNats 18 19 14 13 4
def house : Word := .ofNats 7 14 20 18 4
def plant : Word := .ofNats 15 11 0 13 19
def query : Word := .ofNats 16 20 4 17 24
def bread : Word := .ofNats 1 17 4 0 3

theorem markPairs_length (pairs : List (Fin 26 × Fin 26)) (rem : Fin 26 → Nat) :
    (markPairs pairs rem).length = pairs.length := by
  induction pairs generalizing rem with
  | nil => rfl
  | cons pair rest ih =>
    cases pair with
    | mk s g =>
      by_cases hs : s = g
      · simp [markPairs, hs, ih]
      · by_cases hr : rem g > 0
        · simp [markPairs, hs, hr, ih]
        · simp [markPairs, hs, hr, ih]

theorem letters_length (w : Word) : (lettersOf w).length = 5 := by
  rfl

theorem mark_length (secret guess : Word) : (mark secret guess).length = 5 := by
  simp [mark, markPairs_length, lettersOf]

private theorem headBool (marks : List Mark) (pairs : List (Fin 26 × Fin 26))
    (m : Mark) (s g : Fin 26) (restM : List Mark) (restP : List (Fin 26 × Fin 26))
    (hm : marks = m :: restM) (hp : pairs = (s, g) :: restP) :
    (marks.zip pairs).all (fun item => (item.1 == .green) == (item.2.1 == item.2.2)) =
      (((m == .green) == (s == g)) &&
        (restM.zip restP).all (fun item => (item.1 == .green) == (item.2.1 == item.2.2))) := by
  subst hm
  subst hp
  rfl

theorem markPairs_green (pairs : List (Fin 26 × Fin 26)) (rem : Fin 26 → Nat) :
    ((markPairs pairs rem).zip pairs).all
      (fun item => (item.1 == .green) == (item.2.1 == item.2.2)) = true := by
  induction pairs generalizing rem with
  | nil => rfl
  | cons pair rest ih =>
    rcases pair with ⟨s, g⟩
    by_cases hs : s = g
    · have hmark : markPairs ((s, g) :: rest) rem = .green :: markPairs rest rem := by
        simp [markPairs, hs]
      rw [headBool (markPairs ((s, g) :: rest) rem) ((s, g) :: rest) .green s g
        (markPairs rest rem) rest hmark rfl]
      simp [hs, ih]
      rfl
    · by_cases hr : rem g > 0
      · have hmark : markPairs ((s, g) :: rest) rem = .yellow :: markPairs rest (bump rem g) := by
          simp [markPairs, hs, hr]
        rw [headBool (markPairs ((s, g) :: rest) rem) ((s, g) :: rest) .yellow s g
          (markPairs rest (bump rem g)) rest hmark rfl]
        have hsg : (s == g) = false := by simpa [beq_eq_false_iff_ne] using hs
        simp [hsg, ih]
        rfl
      · have hmark : markPairs ((s, g) :: rest) rem = .wrong :: markPairs rest rem := by
          simp [markPairs, hs, hr]
        rw [headBool (markPairs ((s, g) :: rest) rem) ((s, g) :: rest) .wrong s g
          (markPairs rest rem) rest hmark rfl]
        have hsg : (s == g) = false := by simpa [beq_eq_false_iff_ne] using hs
        simp [hsg, ih]
        rfl

/-- A mark is green exactly where the letters are equal. -/
theorem mark_green_iff (secret guess : Word) :
    ((mark secret guess).zip ((lettersOf secret).zip (lettersOf guess))).all
      (fun item => (item.1 == .green) == (item.2.1 == item.2.2)) = true := by
  simpa [mark] using
    markPairs_green ((lettersOf secret).zip (lettersOf guess)) (afterGreens secret guess)

theorem fresh_is_a_reset (secret : Word) :
    let core := fresh secret
    core.done = false ∧ core.won = false ∧ core.rewardMilli = 0 ∧ core.turn = 0 ∧
      core.errorCount = 0 ∧ core.guesses = [] := by
  simp [fresh]

def cigarMarksOk : Bool :=
  mark crane cigar == [.green, .wrong, .wrong, .yellow, .yellow] &&
    completionMilli (mark crane cigar) == 400

def llamaMarksOk : Bool :=
  mark alley llama == [.yellow, .green, .yellow, .wrong, .wrong]

def appleGreenOk : Bool :=
  allGreen (mark apple apple) && completionMilli (mark apple apple) == 1000

def winOk : Bool :=
  match step (fresh apple) (.play apple true) with
  | some (core, .played 5 .win) =>
    core.done && core.won && core.rewardMilli == 1000 && core.turn == 1 &&
      core.guesses.length == 1 && core.errorCount == 0
  | _ => false

def ongoingOk : Bool :=
  match step (fresh apple) (.play crane true) with
  | some (core, .played 5 .ongoing) =>
    !core.done && !core.won && core.rewardMilli == 0 && core.turn == 1 &&
      core.guesses.length == 1 && core.errorCount == 0
  | _ => false

def sixOk : Bool :=
  match playAll (fresh apple) [crane, stone, house, plant, query, bread] with
  | some core =>
    core.done && !core.won && core.turn == 6 && core.guesses.length == 6 &&
      core.rewardMilli == completionMilli (mark apple bread)
  | none => false

def firstInvalidOk : Bool :=
  match step (fresh apple) (.malformed .format) with
  | some (core, .retry .format) =>
    !core.done && core.turn == 0 && core.guesses.length == 0 && core.errorCount == 1 &&
      core.rewardMilli == 0
  | _ => false

def secondInvalidOk : Bool :=
  match step (fresh apple) (.malformed .length) with
  | some (once, .retry .length) =>
    match step once (.malformed .format) with
    | some (core, .invalidEnd .format 0) =>
      core.done && !core.won && core.rewardMilli == 0 && core.turn == 0 &&
        core.guesses.length == 0
    | _ => false
  | _ => false

def invalidAfterHitOk : Bool :=
  match step (fresh apple) (.play crane true) with
  | some (played, _) =>
    match step played (.malformed .format) with
    | some (once, .retry .format) =>
      match step once (.play cigar false) with
      | some (core, .invalidEnd .notEnglish reward) =>
        core.done && !core.won && core.turn == 1 && core.guesses.length == 1 &&
          reward == completionMilli (mark apple crane) &&
          core.rewardMilli == reward
      | _ => false
    | _ => false
  | _ => false

def repeatOk : Bool :=
  match step (fresh apple) (.play crane true) with
  | some (played, _) =>
    match step played (.play crane false) with
    | some (core, .retry .repeat_) =>
      !core.done && core.turn == 1 && core.guesses.length == 1 && core.errorCount == 1 &&
        core.rewardMilli == 0
    | _ => false
  | _ => false

def refusedAfterWinOk : Bool :=
  match step (fresh apple) (.play apple true) with
  | some (won, _) => step won (.play crane true) == none
  | none => false

theorem cigar_on_crane : cigarMarksOk = true := by native_decide
theorem llama_on_alley : llamaMarksOk = true := by native_decide
theorem all_green_reward : appleGreenOk = true := by native_decide
theorem win_on_first_guess : winOk = true := by native_decide
theorem ongoing_guess_scores_zero : ongoingOk = true := by native_decide
theorem six_misses_terminate : sixOk = true := by native_decide
theorem first_invalid_does_not_spend_a_guess : firstInvalidOk = true := by native_decide
theorem second_invalid_with_no_guess_is_zero : secondInvalidOk = true := by native_decide
theorem invalid_after_a_hit_uses_that_guess : invalidAfterHitOk = true := by native_decide
theorem repeat_is_before_dictionary : repeatOk = true := by native_decide
theorem step_after_win_is_refused : refusedAfterWinOk = true := by native_decide

end SenvWordle
