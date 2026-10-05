// The TV picture on a canvas through WebGL: the decoder's Y, Cb and Cr planes as three textures,
// converted to RGB in the shader with the stream's colour matrix (BT.709 for HD unless it says
// otherwise, BT.601 for SD; video range), an interlaced frame's lines blended with their
// neighbours (no combing on motion), and the display aspect ratio kept, the rest black.

const VERTEX = `
attribute vec2 a;
varying vec2 t;
uniform vec2 crop;
void main() {
  t = vec2((a.x + 1.0) * 0.5 * crop.x, (1.0 - a.y) * 0.5 * crop.y);
  gl_Position = vec4(a, 0.0, 1.0);
}`;

const FRAGMENT = `
precision mediump float;
varying vec2 t;
uniform sampler2D ty, tu, tv;
uniform mat3 m;
uniform float blend;
uniform float row;
void main() {
  vec2 n = vec2(0.0, row);
  float y = mix(texture2D(ty, t).r, texture2D(ty, t + n).r, blend);
  float u = mix(texture2D(tu, t).r, texture2D(tu, t + n).r, blend);
  float v = mix(texture2D(tv, t).r, texture2D(tv, t + n).r, blend);
  vec3 yuv = vec3((y - 16.0 / 255.0) * 255.0 / 219.0, (u - 128.0 / 255.0) * 255.0 / 224.0, (v - 128.0 / 255.0) * 255.0 / 224.0);
  gl_FragColor = vec4(clamp(m * yuv, 0.0, 1.0), 1.0);
}`;

// Y'CbCr to R'G'B' (columns: Y, Cb, Cr) from Kr and Kb.
function matrix(kr, kb) {
  const kg = 1 - kr - kb;
  return new Float32Array([
    1, 1, 1,
    0, -2 * (1 - kb) * kb / kg, 2 * (1 - kb),
    2 * (1 - kr), -2 * (1 - kr) * kr / kg, 0,
  ]);
}
const BT709 = matrix(0.2126, 0.0722);
const BT601 = matrix(0.299, 0.114);

// Display aspect ratio from aspect_ratio_information (H.262 Table 6-3); 1 is square samples.
const DAR = { 2: 4 / 3, 3: 16 / 9, 4: 2.21 };

export class Screen {
  constructor(canvas) {
    this.canvas = canvas;
    const gl = this.gl = canvas.getContext('webgl', { antialias: false, depth: false, preserveDrawingBuffer: false });
    if (!gl) throw new Error('this browser has no WebGL');
    const shader = (type, src) => {
      const s = gl.createShader(type);
      gl.shaderSource(s, src);
      gl.compileShader(s);
      if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
      return s;
    };
    const p = this.program = gl.createProgram();
    gl.attachShader(p, shader(gl.VERTEX_SHADER, VERTEX));
    gl.attachShader(p, shader(gl.FRAGMENT_SHADER, FRAGMENT));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
    gl.useProgram(p);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
    const a = gl.getAttribLocation(p, 'a');
    gl.enableVertexAttribArray(a);
    gl.vertexAttribPointer(a, 2, gl.FLOAT, false, 0, 0);
    this.textures = ['ty', 'tu', 'tv'].map((name, i) => {
      const tex = gl.createTexture();
      gl.activeTexture(gl.TEXTURE0 + i);
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      gl.uniform1i(gl.getUniformLocation(p, name), i);
      return tex;
    });
    this.u = Object.fromEntries(['crop', 'm', 'blend', 'row'].map(n => [n, gl.getUniformLocation(p, n)]));
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    this.size = null;
  }

  // One picture from the Worker: { y, cb, cr, stride, rows, width, height, flags }.
  draw(f) {
    const gl = this.gl;
    const sizes = [[f.stride, f.rows], [f.stride / 2, f.rows / 2], [f.stride / 2, f.rows / 2]];
    const fresh = !this.size || this.size[0] !== f.stride || this.size[1] !== f.rows;
    [f.y, f.cb, f.cr].forEach((plane, i) => {
      gl.activeTexture(gl.TEXTURE0 + i);
      gl.bindTexture(gl.TEXTURE_2D, this.textures[i]);
      const [w, h] = sizes[i];
      if (fresh) gl.texImage2D(gl.TEXTURE_2D, 0, gl.LUMINANCE, w, h, 0, gl.LUMINANCE, gl.UNSIGNED_BYTE, new Uint8Array(plane));
      else gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, w, h, gl.LUMINANCE, gl.UNSIGNED_BYTE, new Uint8Array(plane));
    });
    this.size = [f.stride, f.rows];
    const progressive = (f.flags & 1) !== 0;
    const matrixCode = (f.flags >> 8) & 255;
    const aspect = (f.flags >> 16) & 255;
    // Table 6-9: 1 BT.709; 5, 6 BT.601; unsaid, by the picture's size.
    const bt709 = matrixCode === 1 || (matrixCode !== 5 && matrixCode !== 6 && f.height >= 720);
    gl.uniform2f(this.u.crop, f.width / f.stride, f.height / f.rows);
    gl.uniformMatrix3fv(this.u.m, false, bt709 ? BT709 : BT601);
    gl.uniform1f(this.u.blend, progressive ? 0 : 0.5);
    gl.uniform1f(this.u.row, 1 / f.rows);
    // The canvas at its displayed size; the picture fitted inside at its aspect ratio.
    const c = this.canvas;
    const dpr = window.devicePixelRatio || 1;
    const cw = Math.round(c.clientWidth * dpr), ch = Math.round(c.clientHeight * dpr);
    if (c.width !== cw || c.height !== ch) { c.width = cw; c.height = ch; }
    const dar = DAR[aspect] || f.width / f.height;
    let w = cw, h = Math.round(cw / dar);
    if (h > ch) { h = ch; w = Math.round(ch * dar); }
    gl.viewport(0, 0, cw, ch);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.viewport(Math.round((cw - w) / 2), Math.round((ch - h) / 2), w, h);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  }
}
