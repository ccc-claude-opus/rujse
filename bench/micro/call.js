function add(a, b) { return a + b; }
function f(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = add(s, 1);
  return s;
}
if (f(__N__) !== __N__) throw new Error('call');
