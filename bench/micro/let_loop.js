function f(n) {
  let x = 0;
  for (let k = 0; k < n; k++) x += k;
  return x;
}
if (f(__N__) !== __N__ * (__N__ - 1) / 2) throw new Error('let_loop');
