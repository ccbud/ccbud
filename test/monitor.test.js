'use strict';

const { createMonitorStore } = require('../src/main/monitor');

let pass = 0, fail = 0;
const check = (n, c, d) => { if (c) { pass++; console.log(`  \x1b[32mPASS\x1b[0m ${n}`); } else { fail++; console.log(`  \x1b[31mFAIL\x1b[0m ${n}${d ? ' — ' + d : ''}`); } };

const m = createMonitorStore({ max: 3 });

check('starts empty', m.size() === 0);

m.record({ id: 1, reqBody: { text: 'a' } });
m.record({ id: 2 });
check('get by numeric id', !!(m.get(1) && m.get(1).reqBody.text === 'a'));
check('get by string id', !!m.get('1'));
check('size 2', m.size() === 2, `size=${m.size()}`);

m.record({ id: 3 });
m.record({ id: 4 }); // ring overflow → oldest (1) evicted
check('ring buffer evicts oldest', m.get(1) === null, 'id 1 should be gone');
check('size capped at max', m.size() === 3, `size=${m.size()}`);
check('newest retained', !!m.get(4));

m.record({ id: 4, status: 200 }); // update existing id in place
check('update-in-place does not grow size', m.size() === 3, `size=${m.size()}`);
check('update-in-place overwrites', m.get(4).status === 200);

m.record({}); // missing id ignored
m.record(null); // null ignored
check('record without id is ignored', m.size() === 3, `size=${m.size()}`);

m.clear();
check('clear empties store', m.size() === 0 && m.get(4) === null);

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
