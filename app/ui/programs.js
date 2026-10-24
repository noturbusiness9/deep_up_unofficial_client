// Базовые режимы, доступные каждому владельцу кубика (тариф «Базовый»).
// Остальные режимы приложение не знает: их ID пользователь «обучает» сам —
// запускает режим из официального приложения, и Cube Remote запоминает его ID
// (см. docs/RECOVER_YOUR_PROGRAMS.md).
window.BASIC_PROGRAMS = [
  { key: 'b1', id: 1, name: 'Просто сон', icon: 'just-sleep' },
  { key: 'b3', id: 3, name: 'Дрим кит', icon: 'whale' },
  { key: 'b10', id: 10, name: 'Сон медведя', icon: 'bear' },
  { key: 'b11', id: 11, name: 'Ловец снов', icon: 'dreamcatcher' },
];

window.POWER = { min: 0x00, medium: 0x7f, max: 0xff };
