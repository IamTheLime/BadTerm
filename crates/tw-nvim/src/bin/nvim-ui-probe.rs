use rmpv::Value;
use tw_nvim::{NvimEvent, NvimProcess, RequestId, ui::UiState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut nvim = NvimProcess::spawn()?;
    let attach = nvim.request(
        "nvim_ui_attach",
        vec![
            Value::from(40u64),
            Value::from(10u64),
            Value::Map(vec![(Value::from("rgb"), Value::Boolean(true)), (Value::from("ext_linegrid"), Value::Boolean(true))]),
        ],
    )?;
    let mut ui = UiState::default();
    wait_for_response(&mut nvim, attach, &mut ui)?;

    let new_buffer = nvim.request("nvim_command", vec![Value::from("enew")])?;
    wait_for_response(&mut nvim, new_buffer, &mut ui)?;
    let input = nvim.request("nvim_input", vec![Value::from("iembedded")])?;
    let input_result = wait_for_response(&mut nvim, input, &mut ui)?;
    if input_result.as_u64().unwrap_or(0) == 0 {
        return Err("Neovim accepted no input".into());
    }
    let set_line = nvim.request(
        "nvim_buf_set_lines",
        vec![
            Value::from(0i64),
            Value::from(0i64),
            Value::from(-1i64),
            Value::Boolean(true),
            Value::Array(vec![Value::from("embedded")]),
        ],
    )?;
    wait_for_response(&mut nvim, set_line, &mut ui)?;
    let resize = nvim.request("nvim_ui_try_resize", vec![Value::from(60u64), Value::from(12u64)])?;
    wait_for_response(&mut nvim, resize, &mut ui)?;

    nvim.notify("nvim_command", vec![Value::from("qa!")])?;
    let status = nvim.wait()?;
    let main_grid = ui.grids.get(&1).ok_or("Neovim did not create grid 1")?;
    let lines: Vec<String> = main_grid.cells.iter().map(|line| line.join("")).collect();
    if !lines.iter().any(|line| line.contains("embedded")) {
        return Err(format!("text did not reach the embedded UI: {lines:?}").into());
    }
    println!("embedded Neovim: {}x{}, lines={lines:?}, flushes={}, exit={status}", main_grid.width, main_grid.height, ui.flushes);
    Ok(())
}

fn wait_for_response(nvim: &mut NvimProcess, request: RequestId, ui: &mut UiState) -> Result<Value, Box<dyn std::error::Error>> {
    loop {
        match nvim.recv()? {
            NvimEvent::Response { id, error, result } if id == request => {
                if !error.is_nil() {
                    return Err(format!("Neovim request {request} failed: {error:?}").into());
                }
                return Ok(result);
            }
            NvimEvent::Notification { method, params } if method == "redraw" => ui.apply_redraw(&params)?,
            NvimEvent::ProtocolError(error) => return Err(error.into()),
            _ => {}
        }
    }
}
