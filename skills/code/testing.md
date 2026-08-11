---
name: Testing
description: When you are writing or evaluating tests and want the standard for what makes a good test — testing behavior not implementation, regression value, naming, and avoiding meaningless tests
---

# Testing — Reference

A test earns its maintenance cost by catching regressions — changes that
break behavior the test verifies. This skill is the standard for what makes a
test worth writing and keeping. It provides the criteria for the testability
dimension of `code/design-thinking`. Common sense says "write tests"; this
skill adds the signals for tests that provide real regression protection
versus tests that add maintenance burden without value. It is also the safety net that `code/refactoring` relies on to verify behavior preservation.

## Test behavior, not implementation

A test should verify what the code does, not how it does it. Tests coupled to
internal state, private methods, or implementation details break when the
implementation changes even if the behavior is preserved — they make
refactoring expensive without adding correctness guarantees.

- Test through the public interface: inputs in, observable outputs out.
- Assert on results, not on the sequence of internal calls or the state of
  private fields.
- If a refactoring would change the implementation without changing
  behavior, the test should still pass.

*Avoid:* testing private methods or internal data structures directly —
because coupling a test to implementation makes refactoring a breaking
change, and the test provides no additional correctness guarantee over
testing the public interface. If an internal component is complex enough to
warrant its own tests, extract it behind a testable interface rather than
reaching into private state.

## Regression value

A test is valuable when it would catch a real regression — a change that
breaks the behavior the test verifies. A test that cannot fail provides no
protection.

- A test whose assertion is tautological (`assert_eq!(x, x)`, or asserting a
  value you just set without transformation) cannot fail and provides no
  protection.
- A test for behavior that cannot change (e.g., testing that a constructor
  returns a non-null object in a language where constructors cannot return
  null) has no regression value.
- The more likely a regression is in the tested behavior, the more valuable
  the test.

*Avoid:* writing tests to achieve coverage metrics — because coverage
measures execution, not assertion; a test that calls a function without
meaningful assertions hits coverage without catching regressions.

## Test naming

Test names should describe the behavior being verified, not the mechanism
being exercised. A reader scanning test names should understand what the
code guarantees, not what the test happens to do internally.

- Behavior-focused names: `rejects_negative_amounts`,
  `returns_empty_list_when_no_matches`, `retries_on_transient_failure`
- Implementation-focused names (avoid): `test_processor`,
  `test_method_42`, `test_case_3`

*Avoid:* generic test names that describe the unit under test but not the
scenario — because a test named `test_parser` tells the reader nothing about
what the parser guarantees or what would break if it regressed.

## Over-mocking

Mocks are a tool for isolating the unit under test from its dependencies, but
they become a liability when they encode expectations about how the code
interacts with its collaborators rather than what the code produces. A test
whose mocks duplicate the implementation's call sequence passes when the
mocks match the code but breaks on any refactoring regardless of
correctness — it is testing implementation, not behavior.

- Prefer testing with real collaborators (or fakes that simulate behavior)
  when feasible, because they verify the integration, not just the call
  sequence.
- Use mocks to stub external dependencies (databases, network, time) whose
  real behavior is slow or non-deterministic, not to assert on internal call
  sequences.
- If removing a mock would make the test exercise more real code without
  making it flaky, remove it.

*Avoid:* asserting on the exact sequence or count of calls to a collaborator
— because that couples the test to the implementation's execution path;
assert on the observable outcome instead.

## Decision rules

- If a test would not fail when the behavior it claims to verify is broken,
  the test has no value — remove or rewrite it, because it adds maintenance
  cost without regression protection.
- If a test breaks on every refactoring regardless of whether behavior
  changed, the test is coupled to implementation — rewrite it to test
  through the public interface, because implementation-coupled tests make
  refactoring expensive without adding correctness guarantees.
- If you are writing a test for a method with no branching logic (a
  pure pass-through), skip it or test at a higher level, because a pure
  pass-through cannot regress in your logic.
- If a test name does not describe a scenario or behavior, rename it,
  because a test name is documentation of what the code guarantees.

## Anti-patterns

- **Testing the language** — asserting that a collection grows when you push
  to it, or that a string contains a substring you just set, because these
  test the framework, not your logic; the regression cannot happen in your
  code.
- **Assertion-free tests** — exercising code without asserting outcomes,
  because the test provides execution coverage but no correctness guarantee;
  it will always pass, even when the behavior breaks.
- **Tautological tests** — assertions that are true by construction, because
  the test cannot fail and provides no regression protection.
- **Brittle mocks** — mocks that encode the implementation's call sequence,
  because the test passes when the code matches the mock's expectations and
  breaks on any refactoring regardless of correctness.
