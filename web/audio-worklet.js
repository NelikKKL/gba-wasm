// Воспроизведение звука эмулятора: принимает стерео f32 на 32768 Гц, ресемплирует
// линейной интерполяцией в частоту AudioContext и сама подстраивает скорость по
// заполнению буфера, чтобы не накапливался дрейф между кадрами эмулятора и звуком.
const SRC_RATE = 32768;

class GbaAudio extends AudioWorkletProcessor {
  constructor() {
    super();
    this.size = 1 << 16; // кадров в кольцевом буфере
    this.mask = this.size - 1;
    this.l = new Float32Array(this.size);
    this.r = new Float32Array(this.size);
    this.w = 0; // записано кадров
    this.pos = 0; // позиция чтения (дробная, в кадрах источника)
    this.step = SRC_RATE / sampleRate;
    this.sum = 0;
    this.count = 0;
    this.port.onmessage = e => {
      const d = e.data;
      for (let i = 0; i + 1 < d.length; i += 2) {
        this.l[this.w & this.mask] = d[i];
        this.r[this.w & this.mask] = d[i + 1];
        this.w++;
      }
    };
  }

  process(inputs, outputs) {
    const out = outputs[0];
    const L = out[0];
    const R = out[1] || out[0];
    const target = SRC_RATE * 0.05; // держим ~50 мс запаса
    for (let i = 0; i < L.length; i++) {
      let avail = this.w - this.pos;
      if (avail > SRC_RATE * 0.25) {
        this.pos = this.w - target; // сильно отстали — пропускаем накопившееся
        avail = target;
      }
      if (avail < 2) {
        L[i] = 0;
        if (R !== L) R[i] = 0;
        continue;
      }
      const k = Math.floor(this.pos);
      const f = this.pos - k;
      const k0 = k & this.mask;
      const k1 = (k + 1) & this.mask;
      const l = this.l[k0] + (this.l[k1] - this.l[k0]) * f;
      const r = this.r[k0] + (this.r[k1] - this.r[k0]) * f;
      L[i] = l;
      if (R !== L) R[i] = r;
      const adj = 1 + Math.max(-0.02, Math.min(0.02, ((avail - target) / SRC_RATE) * 0.5));
      this.pos += this.step * adj;
      this.sum += l * l + r * r;
      this.count += 2;
    }
    if (this.count >= sampleRate / 4) {
      this.port.postMessage({ rms: Math.sqrt(this.sum / this.count) });
      this.sum = 0;
      this.count = 0;
    }
    return true;
  }
}

registerProcessor('gba-audio', GbaAudio);
