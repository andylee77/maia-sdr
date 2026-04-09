//
// Fishball P25 - IQPacker cocotb testbench wrapper (Phase 6C)
//
// SPDX-License-Identifier: MIT
//

module tb
  (
   input wire        clk,
   input wire        rst,
   input wire [15:0] re_in,
   input wire [15:0] im_in,
   input wire        strobe_in,
   input wire        stream_ready,
   output wire [63:0] data_out,
   output wire       data_valid,
   output wire       overflow
   );

   dut dut
     (.clk(clk), .rst(rst),
      .re_in(re_in), .im_in(im_in), .strobe_in(strobe_in),
      .stream_ready(stream_ready),
      .data_out(data_out), .data_valid(data_valid), .overflow(overflow));

`ifdef COCOTB_SIM
   initial begin
      $dumpfile("dump.vcd");
      $dumpvars(0, dut);
   end
`endif
endmodule // tb
