$errorMessage = "Manifests not found, make sure to download a small portion of the game and run the script from inside the game folder `nRolling back changes..."

$gameName = Split-Path -Path (Get-Location) -Leaf

Start-Transaction
Get-ChildItem "C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests\Pending" |
Foreach-Object {
    $content = Get-Content $_.FullName | ConvertFrom-Json

    $folderName = $content.MandatoryAppFolderName
    if($gameName -eq $folderName) {
        Write-Host "Modifying $_"

        $manifestLocation = $content.ManifestLocation
        $fileID = $content.InstallationGuid

        $content.bIsIncompleteInstall = $false 
        $content | ConvertTo-Json | Set-Content $_.FullName
        $_ | Move-Item -Destination ($_).Directory.Parent.FullName -Force

        Try{
              Write-Host "Deleting $($manifestLocation)\bps"
              Remove-Item "$($manifestLocation)\bps" -Recurse -Force -ErrorAction Stop
          }
        Catch{
              Undo-Transaction
              throw $errorMessage
          }

        Get-ChildItem "$($manifestLocation)\Pending" |
        Foreach-Object {
          if($_.BaseName -eq $fileID){
                Write-Host "Moving File $_"
                $_ | Move-Item -Destination ($_).Directory.Parent -Force
            }
        }
        Complete-Transaction
        Write-Host "Done!"
        Read-Host "Press any key to exit"
        Exit
    }
}
Undo-Transaction
throw $errorMessage
