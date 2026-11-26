function f(n) {
  const p = { x: 0, y: 1 };
  for (let i = 0; i < n; i++) p.x = p.x + p.y;
  return p.x;
}
if (f(__N__) !== __N__) throw new Error('prop');
