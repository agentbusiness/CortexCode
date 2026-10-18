Before writing a type, module or function: shape the data so every operation can do what it must.

## Steps

1. **Contract per operation.** Every operation the request and the project name, one line each:
   - what it takes;
   - what it returns, and how: a copy, an owned value, or a reference to stored data (read-only or changeable);
   - what it changes;
   - how it fails, in the project's own style (exception, result type, error message);
   - a cost the request states (constant time, one pass).

   Signatures come from the project (existing code, callers, tests), never from a name alone.
2. **Choose the representation from the operations.**
   - Lookup by key: a map. Order: a list. Both ends: a deque. Smallest or largest first: a heap. Uniqueness: a set.
   - An operation that returns stored data by reference: the store owns that data and can lend it.
   - Data reached and changed from several places: one owner; the others reach it through the owner. The language guide names the constructs for this.
   - Invariants: what is always true between operations (a count matches the elements, both ends agree). Write them as comments beside the declaration.
   - Illegal states impossible: one field for a state with several values, not flags that can conflict; required fields always set.
   - Building blocks: what the language, its standard library and the project already provide come first (containers, smart pointers, allocation, parsing, dates, text, concurrency). Write your own machinery only when none of them fits.
3. **Check the hardest operation before writing.** Take the operation that returns a reference, removes from the middle, must be fast, or must be safe across threads. Walk it through the representation: what it reads, what it changes, what it returns. When it cannot be done cleanly, change the representation now, before any function is written.
4. **Keep the project's interface.** Existing names, signatures, error types and messages stay exactly as they are; new code follows the style around it.
5. **Build in order.** The smallest operation end to end first, then one operation at a time, each run against its check. Cases for each: empty, one element, at a boundary, duplicates, invalid input, full size.
6. **When it keeps failing.** The same error after two changes of the line it points at: its cause is a declaration that line uses (a field's type, a signature, the container). Change the representation, not the line.
7. **Simplest that works.** The smallest representation that supports every operation of step 1. No layers, generics, options or abstractions the request does not need. More that should be done: name it in your reply for the user to decide.

## Before finishing

- Every operation of step 1 has been run, with its cases.
- The invariants hold after every operation that changes data.
- Resources are released: memory, files, connections, threads.
- Nothing beyond the request was added.

A choice the user should make (designs with different costs): name the options in your reply. The user asks to weigh options: `skill://skills-engineering/ideation`. A whole new program (layers, scaffolding, build order): `skill://skills-engineering/designing-new-software`.
