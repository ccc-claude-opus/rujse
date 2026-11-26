function f(n) {
  const a = [];
  for (let i = 0; i < n; i++) a.push(i);
  let s = 0;
  for (let i = 0; i < a.length; i++) s += a[i];
  return s;
}
if (f(__N__) !== __N__ * (__N__ - 1) / 2) throw new Error('array');
