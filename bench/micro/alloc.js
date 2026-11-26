function f(n) {
  let last = null;
  for (let i = 0; i < n; i++) last = { i: i, next: last === null ? null : last.i };
  return last.i;
}
if (f(__N__) !== __N__ - 1) throw new Error('alloc');
