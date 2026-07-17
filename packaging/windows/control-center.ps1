#Requires -Version 5.1
param([string]$BinPath = (Join-Path $PSScriptRoot 'sysopt.exe'))
$ErrorActionPreference = 'SilentlyContinue'
Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase

[xml]$xaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        Title="SysOpt Control Center" Height="560" Width="620"
        WindowStartupLocation="CenterScreen" ResizeMode="NoResize">
  <Grid Margin="18">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="145"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/>
      <RowDefinition Height="*"/>
    </Grid.RowDefinitions>
    <TextBlock Text="SysOpt" FontSize="28" FontWeight="Bold"/>
    <Border Grid.Row="1" Margin="0,12,0,12" Padding="12" BorderThickness="1" CornerRadius="8" BorderBrush="#808080">
      <TextBlock Name="StatusText" TextWrapping="Wrap" FontFamily="Consolas" FontSize="13"/>
    </Border>
    <StackPanel Grid.Row="2" Orientation="Horizontal" HorizontalAlignment="Center">
      <Button Name="StartBtn" Content="Iniciar" Width="95" Margin="4" Padding="8"/>
      <Button Name="PauseBtn" Content="Pausar" Width="95" Margin="4" Padding="8"/>
      <Button Name="ResumeBtn" Content="Reanudar" Width="95" Margin="4" Padding="8"/>
      <Button Name="StopBtn" Content="Detener" Width="95" Margin="4" Padding="8"/>
    </StackPanel>
    <GroupBox Grid.Row="3" Header="Modo de optimización" Margin="0,12,0,0" Padding="8">
      <StackPanel Orientation="Horizontal" HorizontalAlignment="Center">
        <ComboBox Name="ProfileBox" Width="250" Margin="5" Padding="6" SelectedIndex="0">
          <ComboBoxItem Content="Inteligente (recomendado)" Tag="smart"/>
          <ComboBoxItem Content="Ahorro de energía" Tag="eco"/>
          <ComboBoxItem Content="Equilibrado" Tag="balanced"/>
          <ComboBoxItem Content="Máximo rendimiento" Tag="performance"/>
          <ComboBoxItem Content="Juegos" Tag="gaming"/>
          <ComboBoxItem Content="Desarrollo" Tag="development"/>
          <ComboBoxItem Content="Creación multimedia" Tag="creator"/>
          <ComboBoxItem Content="Streaming" Tag="streaming"/>
          <ComboBoxItem Content="Silencioso" Tag="quiet"/>
        </ComboBox>
        <Button Name="ApplyProfileBtn" Content="Aplicar modo" Width="125" Margin="5" Padding="8"/>
      </StackPanel>
    </GroupBox>
    <StackPanel Grid.Row="4" Orientation="Horizontal" HorizontalAlignment="Center" Margin="0,12,0,0">
      <Button Name="DoctorBtn" Content="Diagnóstico" Width="120" Margin="5" Padding="7"/>
      <Button Name="ConfigBtn" Content="Abrir configuración" Width="145" Margin="5" Padding="7"/>
      <Button Name="RefreshBtn" Content="Actualizar" Width="100" Margin="5" Padding="7"/>
    </StackPanel>
    <TextBlock Grid.Row="5" Margin="0,14,0,0" Text="El modo Inteligente aprende solo y mantiene límites de seguridad. Los cambios se aplican en caliente, se confirman por acción y se restauran cuando Windows lo permite." TextWrapping="Wrap" Opacity="0.75"/>
  </Grid>
</Window>
'@

$reader = New-Object System.Xml.XmlNodeReader $xaml
$window = [Windows.Markup.XamlReader]::Load($reader)
$names = 'StatusText','StartBtn','PauseBtn','ResumeBtn','StopBtn','ProfileBox','ApplyProfileBtn','DoctorBtn','ConfigBtn','RefreshBtn'
foreach ($name in $names) { Set-Variable -Name $name -Value $window.FindName($name) }

function Invoke-SysOpt([string[]]$Arguments) {
    if (-not (Test-Path $BinPath)) { return $null }
    $output = & $BinPath @Arguments 2>&1 | Out-String
    return $output.Trim()
}

function Update-Status {
    $json = Invoke-SysOpt @('--status-json')
    try {
        $s = $json | ConvertFrom-Json
        $state = if ($s.running) { 'ACTIVO' } else { 'DETENIDO' }
        if ($s.paused) { $state += ' / PAUSADO' }
        $safe = if ($s.safe_mode) { 'Sí' } else { 'No' }
        $apply = if ($s.apply) { 'Sí' } else { 'No' }
        $ram = [Math]::Round($s.available_memory_bytes / (1024 * 1024))
        $cache = [Math]::Round($s.cache.bytes_warmed / (1024 * 1024), 1)
        $ai = if ($s.intelligence.enabled) { "Sí ($([Math]::Round($s.intelligence.confidence * 100))%)" } else { 'No' }
        $StatusText.Text = "Estado: $state`nPerfil: $($s.profile)   Modo: $($s.effective_mode)`nCPU: $([Math]::Round($s.global_cpu_percent,1))%   RAM libre: $ram MiB`nIA híbrida: $ai   Modelo: $($s.intelligence.semantic_model)   Priorizados: $($s.intelligence.boosted_processes)`nApply: $apply   Modo seguro: $safe   Cambios: $($s.managed_changes)`nSmartCache último ciclo: $cache MiB`n$($s.intelligence.summary)`n$($s.message)"
    } catch {
        $StatusText.Text = "SysOpt no está ejecutándose.`n`nUsa Iniciar para activar la tarea programada."
    }
}

$StartBtn.Add_Click({
    Start-ScheduledTask -TaskName 'SysOpt' -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 500
    Update-Status
})
$PauseBtn.Add_Click({ Invoke-SysOpt @('--pause') | Out-Null; Start-Sleep -Milliseconds 300; Update-Status })
$ResumeBtn.Add_Click({ Invoke-SysOpt @('--resume') | Out-Null; Start-Sleep -Milliseconds 300; Update-Status })
$StopBtn.Add_Click({ Invoke-SysOpt @('--shutdown') | Out-Null; Start-Sleep -Milliseconds 500; Update-Status })
$ApplyProfileBtn.Add_Click({
    $selected = $ProfileBox.SelectedItem
    if ($null -ne $selected) {
        Invoke-SysOpt @('--set-profile', [string]$selected.Tag) | Out-Null
        Start-Sleep -Milliseconds 300
        Update-Status
    }
})
$DoctorBtn.Add_Click({
    $result = Invoke-SysOpt @('--doctor')
    [System.Windows.MessageBox]::Show($result, 'Diagnóstico SysOpt') | Out-Null
})
$ConfigBtn.Add_Click({
    $dir = Join-Path $env:APPDATA 'sysopt'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Start-Process explorer.exe $dir
})
$RefreshBtn.Add_Click({ Update-Status })

$timer = New-Object Windows.Threading.DispatcherTimer
$timer.Interval = [TimeSpan]::FromSeconds(2)
$timer.Add_Tick({ Update-Status })
$timer.Start()
Update-Status
$window.ShowDialog() | Out-Null
