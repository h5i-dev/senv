import ArithKernel
/-!
# What Arith-v0 should do

The file to review. It says, in plain integer arithmetic with no machine
words, overflow checks or arrays, what the environment means: the value of a
problem, what the calculator answers, and when an episode pays. `Theorems.lean`
proves that the extracted Rust kernel does exactly this.
-/
open Aeneas Aeneas.Std arith_kernel

namespace arith_kernel.Spec

/-- One operation on integers. -/
def opVal : Op → Int → Int → Int
  | .Add, a, b => a + b
  | .Sub, a, b => a - b
  | .Mul, a, b => a * b

/-- Digit `i` of the problem, as an integer. -/
def digit (p : Problem) (i : Nat) : Int := (p.digits.val.getD i 0#i64).val

/-- Operator `i` of the problem. -/
def op (p : Problem) (i : Nat) : Op := p.ops.val.getD i .Add

/-- The value we mean: `((d0 o0 d1) o1 d2) ...`, left to right, over the
problem's `n_ops` operators. -/
def value (p : Problem) : Int :=
  (List.range p.n_ops.val).foldl (fun acc i => opVal (op p i) acc (digit p (i + 1))) (digit p 0)

/-- The problems the task is about: 2 to 4 operators, every digit 1 to 9. -/
def Valid (p : Problem) : Prop :=
  2 ≤ p.n_ops.val ∧ p.n_ops.val ≤ 4 ∧ ∀ i < 5, 1 ≤ digit p i ∧ digit p i ≤ 9

/-- The calculator's range: operands within ±10^9. -/
abbrev InRange (x : Int) : Prop := -1000000000 ≤ x ∧ x ≤ 1000000000

end arith_kernel.Spec
