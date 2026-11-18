Rust data that is shared, linked or freed by hand: Rc/RefCell, raw pointers, Send/Sync, variance.

## Steps

1. **Pick the sharing by who reads and who changes.**
   - Shared and read-only: `&T`, `Rc<T>` (`Arc<T>` across threads).
   - Shared and changed: `Rc<RefCell<T>>` (`Arc<Mutex<T>>` or `Arc<RwLock<T>>` across threads).
   - `Cell<T>` only for small `Copy` values: it never lends `&T`. `RefCell<T>` lends through `borrow()` and `borrow_mut()`, checked at run time: two live `borrow_mut()` panic.
2. **Nodes that point at each other and must hand out `&T` or `&mut T`.**
   - Each node is owned through `Box`; the links are `NonNull<Node<T>>` or raw pointers.
   - Every dereference is in an `unsafe` block with its invariant in a `// SAFETY:` comment.
   - The owner frees every node in `Drop`.
3. **Allocate with the standard building blocks.**
   - Behind a raw pointer: `Box::into_raw` to allocate, `Box::from_raw` to take back and free. Never `std::alloc` by hand.
   - `NonNull<T>` for a raw pointer that is never null.
4. **Threads.** `Rc`, `Cell` and `RefCell` make a type neither `Send` nor `Sync`: across threads use `Arc` with `Mutex` or `RwLock`.
5. **Variance.** `NonNull<T>`, `*const T` and `PhantomData<T>` are covariant in `T`; `*mut T` is invariant. A type that owns `T` through a raw pointer holds a `PhantomData<T>` beside it.
6. **Check what the compiler decides with code that compiles only when it holds**, in a test:
   - covariance: `fn _covariant<'a>(x: Owner<&'static str>) -> Owner<&'a str> { x }`;
   - thread safety: `fn _is_send<T: Send>() {}` called as `_is_send::<Owner<i32>>()`.
   - It compiles: the property holds.
7. **Check code that frees memory itself** (`unsafe`, `Box::from_raw`, a `Drop` impl) with an element that counts its drops:
   - `struct Counted<'a>(&'a Cell<usize>)` whose `Drop` adds 1;
   - after each removal and after dropping the whole, the count equals the elements that left: a leak or a double free fails the test.
