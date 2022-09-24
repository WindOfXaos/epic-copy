# A script to transfer Epic launcher games between machines
This script helps with transferring already downloaded games from other machines without downloading extra files or waiting for the verification process which is sometimes prone to errors.

> **Warning**
> USE IT AT YOUR OWN RISK! I am not responsible for the misuse of this script, backup your game data and follow the steps carefully.

> **Note**
> This script is only tested on transferring games download **FROM** Epic launcher **TO** Epic launcher

# Prerequisites
- Game files that are downloaded from Epic launcher and are up-to-date from your machine or your friend's machine.
- Only `script.ps1` from this repository

## Step 1
Open Epic launcher and download the game that you are going to transfer, wait until it downloads approximately 10MB then press X to cancel the install. 
![](/images/step1.png)

## Step 2
Exit Epic launcher and make sure it is not running in the task manager.

## Step 3
Copy all files inside the game folder that you have already downloaded or copied from a friend to the install location that you have chosen previously, copy everything except `.egstore` folder.

## Step 4
Copy `script.ps1` inside the root of the install location. After copying the game files and the script it should be something like this
![](/images/step4.png)

## Step 5
Right-click on the script and press **Run with PowerShell**, after it finishes press any key to close the window.
![](/images/step5.png)

## Step 6
Open Epic launcher and you should see "Launch" instead of resume beneath the game.
