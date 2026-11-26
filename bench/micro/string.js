function f(n) {
  let s = '';
  for (let i = 0; i < n; i++) s += 'ab';
  return s.length;
}
if (f(__N__) !== 2 * __N__) throw new Error('string');
